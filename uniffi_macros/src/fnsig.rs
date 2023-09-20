/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::util::{
    create_metadata_items, ident_to_string, mod_path, try_metadata_value_from_usize,
};
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{spanned::Spanned, FnArg, Ident, Pat, Receiver, ReturnType, Type};

pub(crate) struct FnSignature {
    pub kind: FnKind,
    pub span: Span,
    pub mod_path: String,
    pub ident: Ident,
    pub name: String,
    pub is_async: bool,
    pub receiver: Option<ReceiverArg>,
    pub args: Vec<NamedArg>,
    pub return_ty: TokenStream,
    // Does this the return type look like a result?
    // Only use this in UDL mode.
    // In general, it's not reliable because it fails for type aliases.
    pub looks_like_result: bool,
}

impl FnSignature {
    pub(crate) fn new_function(sig: syn::Signature) -> syn::Result<Self> {
        Self::new(FnKind::Function, sig)
    }

    pub(crate) fn new_method(self_ident: Ident, sig: syn::Signature) -> syn::Result<Self> {
        Self::new(FnKind::Method { self_ident }, sig)
    }

    pub(crate) fn new_constructor(self_ident: Ident, sig: syn::Signature) -> syn::Result<Self> {
        Self::new(FnKind::Constructor { self_ident }, sig)
    }

    pub(crate) fn new_trait_method(
        self_ident: Ident,
        sig: syn::Signature,
        index: u32,
    ) -> syn::Result<Self> {
        Self::new(FnKind::TraitMethod { self_ident, index }, sig)
    }

    pub(crate) fn new(kind: FnKind, sig: syn::Signature) -> syn::Result<Self> {
        let span = sig.span();
        let ident = sig.ident;
        let looks_like_result = looks_like_result(&sig.output);
        let output = match sig.output {
            ReturnType::Default => quote! { () },
            ReturnType::Type(_, ty) => quote! { #ty },
        };
        let is_async = sig.asyncness.is_some();

        if is_async && matches!(kind, FnKind::Constructor { .. }) {
            return Err(syn::Error::new(
                span,
                "Async constructors are not supported",
            ));
        }

        let mut input_iter = sig.inputs.into_iter().map(Arg::try_from).peekable();

        let receiver = input_iter
            .next_if(|a| matches!(a, Ok(a) if a.is_receiver()))
            .map(|a| match a {
                Ok(Arg {
                    kind: ArgKind::Receiver(r),
                    ..
                }) => r,
                _ => unreachable!(),
            });
        let args = input_iter
            .map(|a| {
                a.and_then(|a| match a.kind {
                    ArgKind::Named(named) => Ok(named),
                    ArgKind::Receiver(_) => {
                        Err(syn::Error::new(a.span, "Unexpected receiver argument"))
                    }
                })
            })
            .collect::<syn::Result<Vec<_>>>()?;
        let mod_path = mod_path()?;

        Ok(Self {
            kind,
            span,
            mod_path,
            name: ident_to_string(&ident),
            ident,
            is_async,
            receiver,
            args,
            return_ty: output,
            looks_like_result,
        })
    }

    pub fn return_ffi_converter(&self) -> TokenStream {
        let return_ty = &self.return_ty;
        quote! {
            <#return_ty as ::uniffi::FfiConverter<crate::UniFfiTag>>
        }
    }

    /// Lift expressions for each of our arguments
    pub fn lift_exprs(&self) -> impl Iterator<Item = TokenStream> + '_ {
        self.args
            .iter()
            .map(|a| a.lift_expr(&self.return_ffi_converter()))
    }

    /// Write expressions for each of our arguments
    pub fn write_exprs<'a>(
        &'a self,
        buf_ident: &'a Ident,
    ) -> impl Iterator<Item = TokenStream> + 'a {
        self.args.iter().map(|a| a.write_expr(buf_ident))
    }

    /// Parameters expressions for each of our arguments
    pub fn params(&self) -> impl Iterator<Item = TokenStream> + '_ {
        self.args.iter().map(NamedArg::param)
    }

    /// Name of the scaffolding function to generate for this function
    pub fn scaffolding_fn_ident(&self) -> syn::Result<Ident> {
        let name = &self.name;
        let name = match &self.kind {
            FnKind::Function => uniffi_meta::fn_symbol_name(&self.mod_path, name),
            FnKind::Method { self_ident } | FnKind::TraitMethod { self_ident, .. } => {
                uniffi_meta::method_symbol_name(&self.mod_path, &ident_to_string(self_ident), name)
            }
            FnKind::Constructor { self_ident } => uniffi_meta::constructor_symbol_name(
                &self.mod_path,
                &ident_to_string(self_ident),
                name,
            ),
        };
        Ok(Ident::new(&name, Span::call_site()))
    }

    /// Scaffolding parameters expressions for each of our arguments
    pub fn scaffolding_params(&self) -> impl Iterator<Item = TokenStream> + '_ {
        self.args.iter().map(NamedArg::scaffolding_param)
    }

    /// Generate metadata items for this function
    pub(crate) fn metadata_items(&self) -> syn::Result<TokenStream> {
        let Self {
            name,
            return_ty,
            is_async,
            mod_path,
            ..
        } = &self;
        let args_len = try_metadata_value_from_usize(
            // Use param_lifts to calculate this instead of sig.inputs to avoid counting any self
            // params
            self.args.len(),
            "UniFFI limits functions to 256 arguments",
        )?;
        let arg_metadata_calls = self.args.iter().map(NamedArg::metadata_calls);

        match &self.kind {
            FnKind::Function => Ok(create_metadata_items(
                "func",
                name,
                quote! {
                    ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::FUNC)
                        .concat_str(#mod_path)
                        .concat_str(#name)
                        .concat_bool(#is_async)
                        .concat_value(#args_len)
                        #(#arg_metadata_calls)*
                        .concat(<#return_ty as ::uniffi::FfiConverter<crate::UniFfiTag>>::TYPE_ID_META)
                },
                Some(self.checksum_symbol_name()),
            )),

            FnKind::Method { self_ident } => {
                let object_name = ident_to_string(self_ident);
                Ok(create_metadata_items(
                    "method",
                    &format!("{object_name}_{name}"),
                    quote! {
                        ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::METHOD)
                            .concat_str(#mod_path)
                            .concat_str(#object_name)
                            .concat_str(#name)
                            .concat_bool(#is_async)
                            .concat_value(#args_len)
                            #(#arg_metadata_calls)*
                            .concat(<#return_ty as ::uniffi::FfiConverter<crate::UniFfiTag>>::TYPE_ID_META)
                    },
                    Some(self.checksum_symbol_name()),
                ))
            }

            FnKind::TraitMethod { self_ident, index } => {
                let object_name = ident_to_string(self_ident);
                Ok(create_metadata_items(
                    "method",
                    &format!("{object_name}_{name}"),
                    quote! {
                        ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::TRAIT_METHOD)
                            .concat_str(#mod_path)
                            .concat_str(#object_name)
                            .concat_u32(#index)
                            .concat_str(#name)
                            .concat_bool(#is_async)
                            .concat_value(#args_len)
                            #(#arg_metadata_calls)*
                            .concat(<#return_ty as ::uniffi::FfiConverter<crate::UniFfiTag>>::TYPE_ID_META)
                    },
                    Some(self.checksum_symbol_name()),
                ))
            }

            FnKind::Constructor { self_ident } => {
                let object_name = ident_to_string(self_ident);
                Ok(create_metadata_items(
                    "constructor",
                    &format!("{object_name}_{name}"),
                    quote! {
                        ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::CONSTRUCTOR)
                            .concat_str(#mod_path)
                            .concat_str(#object_name)
                            .concat_str(#name)
                            .concat_value(#args_len)
                            #(#arg_metadata_calls)*
                            .concat(<#return_ty as ::uniffi::FfiConverter<crate::UniFfiTag>>::TYPE_ID_META)
                    },
                    Some(self.checksum_symbol_name()),
                ))
            }
        }
    }

    pub(crate) fn checksum_symbol_name(&self) -> String {
        let name = &self.name;
        match &self.kind {
            FnKind::Function => uniffi_meta::fn_checksum_symbol_name(&self.mod_path, name),
            FnKind::Method { self_ident } | FnKind::TraitMethod { self_ident, .. } => {
                uniffi_meta::method_checksum_symbol_name(
                    &self.mod_path,
                    &ident_to_string(self_ident),
                    name,
                )
            }
            FnKind::Constructor { self_ident } => uniffi_meta::constructor_checksum_symbol_name(
                &self.mod_path,
                &ident_to_string(self_ident),
                name,
            ),
        }
    }
}

pub(crate) struct Arg {
    pub(crate) span: Span,
    pub(crate) kind: ArgKind,
}

pub(crate) enum ArgKind {
    Receiver(ReceiverArg),
    Named(NamedArg),
}

impl Arg {
    pub(crate) fn is_receiver(&self) -> bool {
        matches!(self.kind, ArgKind::Receiver(_))
    }
}

impl TryFrom<FnArg> for Arg {
    type Error = syn::Error;

    fn try_from(syn_arg: FnArg) -> syn::Result<Self> {
        let span = syn_arg.span();
        let kind = match syn_arg {
            FnArg::Typed(p) => match *p.pat {
                Pat::Ident(i) => Ok(ArgKind::Named(NamedArg::new(i.ident, &p.ty))),
                _ => Err(syn::Error::new_spanned(p, "Argument name missing")),
            },
            FnArg::Receiver(receiver) => Ok(ArgKind::Receiver(ReceiverArg::from(receiver))),
        }?;

        Ok(Self { span, kind })
    }
}

pub(crate) enum ReceiverArg {
    Ref,
    Arc,
}

impl From<Receiver> for ReceiverArg {
    fn from(receiver: Receiver) -> Self {
        if let Type::Path(p) = *receiver.ty {
            if let Some(segment) = p.path.segments.last() {
                // This comparison will fail if a user uses a typedef for Arc.  Maybe we could
                // implement some system like TYPE_ID_META to figure this out from the type system.
                // However, this seems good enough for now.
                if segment.ident == "Arc" {
                    return ReceiverArg::Arc;
                }
            }
        }
        Self::Ref
    }
}

pub(crate) struct NamedArg {
    pub(crate) ident: Ident,
    pub(crate) name: String,
    pub(crate) ty: TokenStream,
    pub(crate) ref_type: Option<Type>,
}

impl NamedArg {
    pub(crate) fn new(ident: Ident, ty: &Type) -> Self {
        match ty {
            Type::Reference(r) => {
                let inner = &r.elem;
                Self {
                    name: ident_to_string(&ident),
                    ident,
                    ty: quote! { <#inner as ::uniffi::LiftRef<crate::UniFfiTag>>::LiftType },
                    ref_type: Some(*inner.clone()),
                }
            }
            _ => Self {
                name: ident_to_string(&ident),
                ident,
                ty: quote! { #ty },
                ref_type: None,
            },
        }
    }

    /// Generate the expression for this argument's FfiConverter
    pub(crate) fn ffi_converter(&self) -> TokenStream {
        let ty = &self.ty;
        quote! { <#ty as ::uniffi::FfiConverter<crate::UniFfiTag>> }
    }

    /// Generate the expression for this argument's FfiType
    pub(crate) fn ffi_type(&self) -> TokenStream {
        let ffi_converter = self.ffi_converter();
        quote! { #ffi_converter::FfiType }
    }

    /// Generate the parameter for this Arg
    pub(crate) fn param(&self) -> TokenStream {
        let ident = &self.ident;
        let ty = &self.ty;
        quote! { #ident: #ty }
    }

    /// Generate the scaffolding parameter for this Arg
    pub(crate) fn scaffolding_param(&self) -> TokenStream {
        let ident = &self.ident;
        let ffi_type = self.ffi_type();
        quote! { #ident: #ffi_type }
    }

    /// Generate the expression to lift the scaffolding parameter for this arg
    pub(crate) fn lift_expr(&self, return_ffi_converter: &TokenStream) -> TokenStream {
        let ident = &self.ident;
        let ty = &self.ty;
        let ffi_converter = self.ffi_converter();
        let name = &self.name;
        let lift = quote! {
            match #ffi_converter::try_lift(#ident) {
                Ok(v) => v,
                Err(e) => return Err(#return_ffi_converter::handle_failed_lift(#name, e))
            }
        };
        match &self.ref_type {
            None => lift,
            Some(ref_type) => quote! {
                <#ty as ::std::borrow::Borrow<#ref_type>>::borrow(&#lift)
            },
        }
    }

    /// Generate the expression to write the scaffolding parameter for this arg
    pub(crate) fn write_expr(&self, buf_ident: &Ident) -> TokenStream {
        let ident = &self.ident;
        let ffi_converter = self.ffi_converter();
        quote! { #ffi_converter::write(#ident, &mut #buf_ident) }
    }

    pub(crate) fn metadata_calls(&self) -> TokenStream {
        let name = &self.name;
        let ffi_converter = self.ffi_converter();
        quote! {
            .concat_str(#name)
            .concat(#ffi_converter::TYPE_ID_META)
        }
    }
}

fn looks_like_result(return_type: &ReturnType) -> bool {
    if let ReturnType::Type(_, ty) = return_type {
        if let Type::Path(p) = &**ty {
            if let Some(seg) = p.path.segments.last() {
                if seg.ident == "Result" {
                    return true;
                }
            }
        }
    }

    false
}

#[derive(Debug)]
pub(crate) enum FnKind {
    Function,
    Constructor { self_ident: Ident },
    Method { self_ident: Ident },
    TraitMethod { self_ident: Ident, index: u32 },
}
