// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Kimojio procedural macros.
//!
//! These macros provide convenient attributes for defining async main functions and tests
//! that run within the kimojio runtime.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, parse_macro_input};

/// Marks an async main function to be run with the kimojio runtime.
///
/// This macro transforms an async main function into a regular main function
/// that uses `kimojio::run` to execute the async code.
///
/// # Example
///
/// ```ignore
/// #[kimojio::main]
/// async fn main() {
///     // async main code
/// }
/// ```
///
/// This expands to:
///
/// ```ignore
/// fn main() {
///     kimojio::run(0, async {
///         // async main code
///     });
/// }
/// ```
#[proc_macro_attribute]
pub fn main(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);

    let ItemFn {
        attrs,
        vis,
        sig,
        block,
        ..
    } = input;

    let fn_name = &sig.ident;

    // Check if function is async
    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            sig.fn_token,
            "the async keyword is missing from the function declaration",
        )
        .to_compile_error()
        .into();
    }

    // Check if it's the main function
    if fn_name != "main" {
        return syn::Error::new_spanned(
            fn_name,
            "only the main function can be marked with #[kimojio::main]",
        )
        .to_compile_error()
        .into();
    }

    // Check that main has no parameters
    if !sig.inputs.is_empty() {
        return syn::Error::new_spanned(&sig.inputs, "the main function cannot have parameters")
            .to_compile_error()
            .into();
    }

    // Get the function body without async
    let body = &block;

    // Get the return type from the async function signature
    let return_type = &sig.output;

    // Generate the expanded code
    let expanded = quote! {
        #(#attrs)*
        #vis fn main() #return_type {
            match kimojio::run(0, async #body) {
                // Propagate the body return value
                Some(Ok(output)) => output,
                // Task panicked.
                Some(Err(panic_payload)) => std::panic::resume_unwind(panic_payload),
                None => panic!("Runtime shutdown_loop called"),
            }
        }
    };

    TokenStream::from(expanded)
}

/// Marks an async test to be run with the kimojio runtime.
///
/// This macro transforms an async test function into a regular test function
/// that uses `::kimojio::run_test` to execute the async code.
///
/// # Example
///
/// ```ignore
/// #[kimojio::test]
/// async fn my_test() {
///     // async test code
/// }
/// ```
///
/// To use the virtual clock in a test, call
/// `operations::virtual_clock_enable(true)` at the start of the test body:
///
/// ```ignore
/// use std::time::Duration;
/// use kimojio::operations;
///
/// #[kimojio::test]
/// async fn my_test() {
///     operations::virtual_clock_enable(true);
///     operations::virtual_clock_advance(Duration::from_secs(60));
///     // No real time passes!
/// }
/// ```
#[proc_macro_attribute]
pub fn test(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemFn);

    let ItemFn {
        attrs,
        vis,
        sig,
        block,
        ..
    } = input;

    let fn_name = &sig.ident;
    let fn_name_str = fn_name.to_string();

    // Check if function is async
    if sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            sig.fn_token,
            "the async keyword is missing from the function declaration",
        )
        .to_compile_error()
        .into();
    }

    // Reject any attributes — the macro takes no arguments
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro::Span::call_site().into(),
            "unsupported attribute; expected `#[kimojio::test]` with no arguments",
        )
        .to_compile_error()
        .into();
    }

    // Get the function body without async
    let body = &block;

    let expanded = quote! {
        #[test]
        #(#attrs)*
        #vis fn #fn_name() {
            ::kimojio::run_test(#fn_name_str, async #body)
        }
    };

    TokenStream::from(expanded)
}
