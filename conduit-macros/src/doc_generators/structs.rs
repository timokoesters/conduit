use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, quote};
use syn::{Field, Ident, ItemStruct, parse::Parse};

use super::attrs_to_doc_comment;

pub(crate) struct Struct {
    ident: Ident,
    doc_comment: String,
    fields: Vec<DocField>,
}

struct DocField {
    ident: Ident,
    doc_comment: String,
}

impl Parse for Struct {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ItemStruct {
            ident,
            fields,
            attrs,
            ..
        } = ItemStruct::parse(input)?;

        let fields = fields
            .into_iter()
            // I guess fields can have
            .map(
                |Field {
                     attrs,
                     ident: field_ident,
                     ..
                 }| {
                    let ident = field_ident
                        .ok_or_else(|| syn::Error::new(ident.span(), "Expected named field"))?;
                    let doc_comment = attrs_to_doc_comment(attrs);

                    Ok(DocField { ident, doc_comment })
                },
            )
            .collect::<syn::Result<Vec<_>>>()?;

        let doc_comment = attrs_to_doc_comment(attrs);

        Ok(Self {
            ident,
            fields,
            doc_comment,
        })
    }
}

/// Produces the following functions on the struct:
/// - `field_doc_comments`, returning each field and it's doc comment.
/// - `container_doc_comment`, returning the doc comment of the struct.
impl ToTokens for Struct {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        let Self {
            ident,
            fields,
            doc_comment, /* , doc_comments */
        } = self;
        let output = quote! {
            impl DocumentStruct for #ident {
                fn field_doc_comments() -> Vec<(String, String)> {
                    vec![#((#fields)),*]
                }

                fn container_doc_comment() -> String {
                    #doc_comment.to_owned()
                }
            }
        };

        tokens.extend(output);
    }
}

impl ToTokens for DocField {
    fn to_tokens(&self, tokens: &mut TokenStream2) {
        let Self { ident, doc_comment } = self;

        let str_ident = ident.to_string();

        // `clone` because `to_tokens` takes a reference to self.
        tokens.extend(quote!( (#str_ident.to_owned(), #doc_comment.to_owned() ) ))
    }
}
