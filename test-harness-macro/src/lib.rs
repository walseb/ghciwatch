//! `#[test]` attribute macro for `ghciwatch` integration tests.

#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

use proc_macro::TokenStream;

use quote::quote;
use quote::ToTokens;
use syn::parse;
use syn::parse::Parse;
use syn::parse::ParseStream;
use syn::Attribute;
use syn::Block;
use syn::Ident;
use syn::ItemFn;

/// Runs a test asynchronously in the `tokio` current-thread runtime with `tracing` enabled.
///
/// The test uses the unversioned `ghc` executable present in the runtime environment.
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    // Parse annotated function
    let mut function: ItemFn = parse(item).expect("Could not parse item as function");

    // Add attributes to run the test in the `tokio` current-thread runtime and enable tracing.
    function.attrs.extend(
        parse::<Attributes>(
            quote! {
                #[tokio::test]
                #[tracing_test::traced_test]
                #[allow(non_snake_case)]
            }
            .into(),
        )
        .expect("Could not parse quoted attributes")
        .0,
    );

    if !attr.is_empty() {
        let mode: Ident = parse(attr).expect("Expected `current`");
        if mode != "current" {
            panic!("Unknown test mode `{mode}`; expected `current`");
        }
    }

    make_current_test_fn(function).to_token_stream().into()
}

struct Attributes(Vec<Attribute>);

impl Parse for Attributes {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        Ok(Self(input.call(Attribute::parse_outer)?))
    }
}

fn make_current_test_fn(mut function: ItemFn) -> ItemFn {
    let stmts = function.block.stmts;
    let test_name = function.sig.ident.to_string();
    let new_body = parse::<Block>(
        quote! {
            {
                ::test_harness::internal::wrap_test_with_environment_ghc(
                    async {
                        #(#stmts);*
                    },
                    #test_name,
                    env!("CARGO_TARGET_TMPDIR"),
                ).await;
            }
        }
        .into(),
    )
    .expect("Could not parse function body");
    *function.block = new_body;
    function
}
