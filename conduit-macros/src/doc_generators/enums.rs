use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{Ident, ItemEnum, Variant, parse::Parse};

use super::attrs_to_doc_comment;

pub(crate) struct Enum {
    ident: Ident,
    doc_comment: String,
    variants: Vec<DocVariant>,
}

struct DocVariant {
    ident: Ident,
    doc_comment: String,
}

impl Parse for Enum {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ItemEnum {
            ident,
            variants,
            attrs,
            ..
        } = ItemEnum::parse(input)?;

        let variants = variants
            .into_iter()
            .map(|Variant { attrs, ident, .. }| {
                let doc_comment = attrs_to_doc_comment(attrs);

                Ok(DocVariant { ident, doc_comment })
            })
            .collect::<syn::Result<Vec<_>>>()?;

        let doc_comment = attrs_to_doc_comment(attrs);

        Ok(Self {
            ident,
            variants,
            doc_comment,
        })
    }
}

/// Produces the following functions on the enum:
/// - `variant_doc_comments`, returning each variant and it's doc comment.
/// - `container_doc_comment`, returning the doc comment of the enum.
impl ToTokens for Enum {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        let Self {
            ident,
            variants,
            doc_comment, /* , doc_comments */
        } = self;
        let output = quote! {
            impl DocumentEnum for #ident {
                fn variant_doc_comments() -> Vec<(Self, String)> {
                    vec![#((#variants)),*]
                }

                fn container_doc_comment() -> String {
                    #doc_comment.to_owned()
                }
            }
        };

        tokens.extend(output);
    }
}

impl ToTokens for DocVariant {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        let Self { ident, doc_comment } = self;

        // `clone` because `to_tokens` takes a reference to self.
        tokens.extend(quote!( (Self::#ident, #doc_comment.to_owned() ) ))
    }
}
