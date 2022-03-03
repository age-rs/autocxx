// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod analysis;
mod api;
mod codegen_cpp;
mod codegen_rs;
#[cfg(test)]
mod conversion_tests;
mod convert_error;
mod doc_attr;
mod error_reporter;
mod parse;
mod utilities;

use analysis::fun::FnAnalyzer;
use autocxx_parser::IncludeCppConfig;
pub(crate) use codegen_cpp::CppCodeGenerator;
pub(crate) use convert_error::ConvertError;
use itertools::Itertools;
use syn::{Item, ItemMod};

use crate::{
    conversion::analysis::deps::HasDependencies, CppCodegenOptions, CppFilePair, UnsafePolicy,
};

use self::{
    analysis::{
        abstract_types::{discard_ignored_functions, mark_types_abstract},
        allocators::create_alloc_and_frees,
        casts::add_casts,
        check_names,
        constructor_deps::decorate_types_with_constructor_deps,
        fun::FnPhase,
        gc::filter_apis_by_following_edges_from_allowlist,
        pod::analyze_pod_apis,
        remove_ignored::filter_apis_by_ignored_dependents,
        tdef::convert_typedef_targets,
    },
    api::{AnalysisPhase, Api},
    codegen_rs::RsCodeGenerator,
    parse::ParseBindgen,
};

const LOG_APIS: bool = true;

/// Converts the bindings generated by bindgen into a form suitable
/// for use with `cxx`.
/// In fact, most of the actual operation happens within an
/// individual `BridgeConversion`.
///
/// # Flexibility in handling bindgen output
///
/// autocxx is inevitably tied to the details of the bindgen output;
/// e.g. the creation of a 'root' mod when namespaces are enabled.
/// At the moment this crate takes the view that it's OK to panic
/// if the bindgen output is not as expected. It may be in future that
/// we need to be a bit more graceful, but for now, that's OK.
pub(crate) struct BridgeConverter<'a> {
    include_list: &'a [String],
    config: &'a IncludeCppConfig,
}

/// C++ and Rust code generation output.
pub(crate) struct CodegenResults {
    pub(crate) rs: Vec<Item>,
    pub(crate) cpp: Option<CppFilePair>,
}

impl<'a> BridgeConverter<'a> {
    pub fn new(include_list: &'a [String], config: &'a IncludeCppConfig) -> Self {
        Self {
            include_list,
            config,
        }
    }

    fn dump_apis<T: AnalysisPhase>(label: &str, apis: &[Api<T>]) {
        if LOG_APIS {
            log::info!(
                "APIs after {}:\n{}",
                label,
                apis.iter().map(|api| { format!("  {:?}", api) }).join("\n")
            )
        }
    }

    fn dump_apis_with_deps(label: &str, apis: &[Api<FnPhase>]) {
        if LOG_APIS {
            log::info!(
                "APIs after {}:\n{}",
                label,
                apis.iter()
                    .map(|api| { format!("  {:?}, deps={}", api, api.format_deps()) })
                    .join("\n")
            )
        }
    }

    /// Convert a TokenStream of bindgen-generated bindings to a form
    /// suitable for cxx.
    ///
    /// This is really the heart of autocxx. It parses the output of `bindgen`
    /// (although really by "parse" we mean to interpret the structures already built
    /// up by the `syn` crate).
    pub(crate) fn convert(
        &self,
        mut bindgen_mod: ItemMod,
        unsafe_policy: UnsafePolicy,
        inclusions: String,
        cpp_codegen_options: &CppCodegenOptions,
    ) -> Result<CodegenResults, ConvertError> {
        match &mut bindgen_mod.content {
            None => Err(ConvertError::NoContent),
            Some((_, items)) => {
                // Parse the bindgen mod.
                let items_to_process = items.drain(..).collect();
                let parser = ParseBindgen::new(self.config);
                let apis = parser.parse_items(items_to_process)?;
                Self::dump_apis("parsing", &apis);
                // Inside parse_results, we now have a list of APIs.
                // We now enter various analysis phases.
                // Next, convert any typedefs.
                // "Convert" means replacing bindgen-style type targets
                // (e.g. root::std::unique_ptr) with cxx-style targets (e.g. UniquePtr).
                let apis = convert_typedef_targets(self.config, apis);
                Self::dump_apis("typedefs", &apis);
                // Now analyze which of them can be POD (i.e. trivial, movable, pass-by-value
                // versus which need to be opaque).
                // Specifically, let's confirm that the items requested by the user to be
                // POD really are POD, and duly mark any dependent types.
                // This returns a new list of `Api`s, which will be parameterized with
                // the analysis results. It also returns an object which can be used
                // by subsequent phases to work out which objects are POD.
                let analyzed_apis = analyze_pod_apis(apis, self.config)?;
                Self::dump_apis("pod analysis", &analyzed_apis);
                let analyzed_apis = add_casts(analyzed_apis);
                let analyzed_apis = create_alloc_and_frees(analyzed_apis);
                // Next, figure out how we materialize different functions.
                // Some will be simple entries in the cxx::bridge module; others will
                // require C++ wrapper functions. This is probably the most complex
                // part of `autocxx`. Again, this returns a new set of `Api`s, but
                // parameterized by a richer set of metadata.
                Self::dump_apis("adding casts", &analyzed_apis);
                let analyzed_apis =
                    FnAnalyzer::analyze_functions(analyzed_apis, unsafe_policy, self.config);
                // If any of those functions turned out to be pure virtual, don't attempt
                // to generate UniquePtr implementations for the type, since it can't
                // be instantiated.
                Self::dump_apis("analyze fns", &analyzed_apis);
                let analyzed_apis = mark_types_abstract(analyzed_apis);
                Self::dump_apis("marking abstract", &analyzed_apis);
                // Annotate structs with a note of any copy/move constructors which
                // we may want to retain to avoid garbage collecting them later.
                let analyzed_apis = decorate_types_with_constructor_deps(analyzed_apis);
                Self::dump_apis_with_deps("adding constructor deps", &analyzed_apis);
                let analyzed_apis = discard_ignored_functions(analyzed_apis);
                Self::dump_apis_with_deps("ignoring ignorable fns", &analyzed_apis);
                // Remove any APIs whose names are not compatible with cxx.
                let analyzed_apis = check_names(analyzed_apis);
                // During parsing or subsequent processing we might have encountered
                // items which we couldn't process due to as-yet-unsupported features.
                // There might be other items depending on such things. Let's remove them
                // too.
                let analyzed_apis = filter_apis_by_ignored_dependents(analyzed_apis);
                Self::dump_apis_with_deps("removing ignored dependents", &analyzed_apis);

                // We now garbage collect the ones we don't need...
                let mut analyzed_apis =
                    filter_apis_by_following_edges_from_allowlist(analyzed_apis, self.config);
                // Determine what variably-sized C types (e.g. int) we need to include
                analysis::ctypes::append_ctype_information(&mut analyzed_apis);
                Self::dump_apis_with_deps("GC", &analyzed_apis);
                // And finally pass them to the code gen phases, which outputs
                // code suitable for cxx to consume.
                let cpp = CppCodeGenerator::generate_cpp_code(
                    inclusions,
                    &analyzed_apis,
                    self.config,
                    cpp_codegen_options,
                )?;
                let rs = RsCodeGenerator::generate_rs_code(
                    analyzed_apis,
                    self.include_list,
                    bindgen_mod,
                    self.config,
                    cpp.as_ref().map(|file_pair| file_pair.header_name.clone()),
                );
                Ok(CodegenResults { rs, cpp })
            }
        }
    }
}
