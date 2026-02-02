use quote::{ToTokens, quote};
use syn::{Field as SynField, Ident, ItemStruct, Type, parse::Parse};

pub(crate) struct Struct {
    ident: Ident,
    fields: Vec<Field>,
    field_type: Ident,
}

struct Field(Ident);

impl Parse for Struct {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let ItemStruct { ident, fields, .. } = ItemStruct::parse(input)?;

        let mut field_type = None;

        let fields = fields
            .into_iter()
            .map(
                |SynField {
                     ident: field_ident,
                     ty,
                     ..
                 }| {
                    if let Some(field_type) = &field_type {
                        if ty != *field_type {
                            return Err(syn::Error::new(
                                ident.span(),
                                "Not all fields are of the same type",
                            ));
                        }
                    } else {
                        field_type = Some(ty)
                    }

                    let ident = field_ident
                        .ok_or_else(|| syn::Error::new(ident.span(), "Expected named field"))?;

                    Ok(Field(ident))
                },
            )
            .collect::<syn::Result<Vec<_>>>()?;

        if let Some(Type::Path(field_type)) = field_type {
            Ok(Struct {
                field_type: field_type
                    .path
                    .segments
                    .into_iter()
                    .last()
                    .ok_or_else(|| {
                        syn::Error::new(ident.span(), "Path for field type has no last segment")
                    })?
                    .ident,
                ident,
                fields,
            })
        } else {
            Err(syn::Error::new(ident.span(), "Struct has no fields"))
        }
    }
}

impl ToTokens for Struct {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let Self {
            ident,
            fields,
            field_type,
        } = self;

        let output = quote! {
            impl GetStructFieldValue for #ident {
                type Limitation = #field_type;

                fn get(&self, field: &str) -> Option<Self::Limitation> {
                    match field {
                        #(#fields)*
                        _ => None
                    }
                }
            }
        };

        tokens.extend(output);
    }
}

impl ToTokens for Field {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let Self(ident) = self;
        let str_ident = ident.to_string();

        tokens.extend(quote!(
                                #str_ident => Some(self.#ident),
        ));
    }
}
