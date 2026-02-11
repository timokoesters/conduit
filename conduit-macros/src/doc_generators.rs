use std::path::Path;

use proc_macro::TokenStream;
use syn::{parse::Parse, Attribute, Field, ItemEnum, MetaNameValue, Variant};

pub struct RestrictionInfo {
    kind: Ident,
    comment: Attribute,
}

impl Parse for RestrictionInfo {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let Variant { attrs, ident, .. } = Variant::parse(input)?;

        let comment = attrs.into_iter().filter(|attr| {
            if let syn::Meta::NameValue(MetaNameValue {
                path,
                eq_token,
                value,
            }) = attr.meta
            {
                todo!()
            } else {
                false
            }
        });

        Ok(Self {
            kind: ident,
            comment,
        })
    }
}
