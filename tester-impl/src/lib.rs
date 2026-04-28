#![feature(bool_to_result)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::ToTokens;
use syn::{parse_macro_input, spanned::Spanned};

mod expand;
mod result_fn;

pub(crate) use crate::{expand::handle_block, result_fn::ResultFn};

/// Defer the execution of disabling terminal raw mode by rewriting the routine,
/// and not relying on destructors for a defer object being run.
#[proc_macro_attribute]
pub fn defer_drm(params: TokenStream, func: TokenStream) -> TokenStream {
    if !params.is_empty() {
        return TokenStream::from(
            syn::Error::new(
                TokenStream2::from(params).span(),
                "this macro does not accept parameters",
            )
            .into_compile_error(),
        );
    }

    TokenStream::from(parse_macro_input!(func as ResultFn).to_token_stream())
}
