use proc_macro2::TokenStream as TokenStream2;
use quote::ToTokens;
use syn::{
    ItemFn, Path, PathSegment, ReturnType, Signature, Type, TypePath,
    parse::{Parse, ParseStream},
};

use crate::handle_block;

pub(crate) struct ResultFn(pub(crate) ItemFn);

impl ToTokens for ResultFn {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        let Self(inner) = self;

        inner.to_tokens(tokens);
    }
}

impl Parse for ResultFn {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut func: ItemFn = input.parse()?;
        let ItemFn {
            sig: Signature { output, .. },
            block,
            ..
        } = &mut func;

        ensure_retval(output).ok_or_else(|| {
            input.error("this attribute should only annotate functions returning `anyhow::Result`")
        })?;

        // NOTE: this triggers the process of dual recursion that starts with the
        // function block and processes all expressions within it, including other
        // blocks, but excluding scopes that would not propagate immediately to the
        // function (e.g. the body of a closure.)
        handle_block(block);

        Ok(Self(func))
    }
}
pub(crate) fn ensure_retval(output: &ReturnType) -> bool {
    match output {
        ReturnType::Type(_, ty)
            if let Type::Path(TypePath {
                path: Path { segments, .. },
                ..
            }) = &**ty
                && segments
                    .last()
                    .is_some_and(|PathSegment { ident, .. }| ident == "Result") =>
        {
            true
        }
        _ => false,
    }
}
