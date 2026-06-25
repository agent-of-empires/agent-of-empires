//! `#[derive(SettingsSection)]` for agent-of-empires (#1692).
//!
//! Emits `fn settings_descriptors() -> Vec<FieldDescriptor>` for a config
//! sub-struct so the field declaration plus its `#[setting(...)]` attributes
//! are the single source of truth for every settings surface (TUI, web,
//! server policy, validation). The generated code references
//! `crate::session::settings_schema::*`, so the derive is only meant for use
//! inside the `agent-of-empires` crate.
//!
//! Section attribute (required):
//! ```ignore
//! #[setting_section(name = "acp", category = "Acp")]
//! ```
//!
//! Per-field attribute:
//! ```ignore
//! #[setting(label = "Acp enabled", widget = "toggle")]
//! #[setting(label = "Node path", web = "local_only:host binary execution surface")]
//! #[setting(skip)]   // not a user-facing setting
//! ```
//! Keys: `label`, `desc`, `category` (override the section default), `widget`,
//! `min`, `max`, `step`, `multiline`, `mono`, `options` ("v:Label,v2:Label2"),
//! `web` ("allow" | "elevation:reason" | "local_only:reason"),
//! `validate` ("none" | "range:min[:max]" | "nonempty" | "memory_limit" |
//!   "volume_list" | "env_list" | "port_mapping_list"),
//! `global_only` (flag: field is shown but not profile-overridable),
//! `skip` (flag: exclude the field from the schema entirely).
//! When `desc` is omitted, the field's doc comment is used.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitInt, LitStr};

#[proc_macro_derive(SettingsSection, attributes(setting_section, setting))]
pub fn derive_settings_section(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

struct SectionMeta {
    name: String,
    category: String,
}

fn expand(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let ident = &input.ident;
    let section = parse_section_meta(&input)?;
    let section_name = &section.name;
    let default_category = &section.category;

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    ident,
                    "SettingsSection requires named fields",
                ))
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                ident,
                "SettingsSection can only derive on structs",
            ))
        }
    };

    let mut descriptors = Vec::new();
    for field in fields {
        let field_ident = field.ident.as_ref().expect("named field");
        let field_name = field_ident.to_string();
        let attrs = parse_field_attrs(field, &field_name)?;
        if attrs.skip {
            continue;
        }
        let widget = build_widget(field, &attrs)?;
        let web = build_web(field, &attrs)?;
        let validation = build_validation(field, &attrs)?;
        let overridable = !attrs.global_only;
        let advanced = attrs.advanced;
        let label = attrs.label.unwrap_or_else(|| humanize(&field_name));
        let description = attrs
            .desc
            .or_else(|| doc_comment(field))
            .unwrap_or_default();
        let category = attrs.category.unwrap_or_else(|| default_category.clone());

        descriptors.push(quote! {
            FieldDescriptor {
                section: #section_name.to_string(),
                field: #field_name.to_string(),
                category: #category.to_string(),
                label: #label.to_string(),
                description: #description.to_string(),
                widget: #widget,
                web_write: #web,
                profile_overridable: #overridable,
                validation: #validation,
                advanced: #advanced,
                // Core fields always exist in the serialized Config via the
                // struct Default, so they need no schema-carried default; only
                // plugin fields set this.
                default: ::core::option::Option::None,
            }
        });
    }

    Ok(quote! {
        impl #ident {
            /// Schema descriptors for this section, emitted by
            /// `#[derive(SettingsSection)]`. The single source of truth for
            /// how every surface renders and guards these fields.
            pub fn settings_descriptors() -> ::std::vec::Vec<crate::session::settings_schema::FieldDescriptor> {
                use crate::session::settings_schema::{
                    FieldDescriptor, WidgetKind, WebWritePolicy, ValidationKind, SelectOption,
                };
                ::std::vec![ #(#descriptors),* ]
            }
        }
    })
}

fn parse_section_meta(input: &DeriveInput) -> syn::Result<SectionMeta> {
    let mut name = None;
    let mut category = None;
    for attr in &input.attrs {
        if !attr.path().is_ident("setting_section") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                name = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("category") {
                category = Some(meta.value()?.parse::<LitStr>()?.value());
            } else {
                return Err(meta.error("unknown setting_section key"));
            }
            Ok(())
        })?;
    }
    let name = name.ok_or_else(|| {
        syn::Error::new_spanned(input, "missing #[setting_section(name = \"...\")]")
    })?;
    let category = category.ok_or_else(|| {
        syn::Error::new_spanned(input, "missing #[setting_section(category = \"...\")]")
    })?;
    Ok(SectionMeta { name, category })
}

#[derive(Default)]
struct FieldAttrs {
    skip: bool,
    global_only: bool,
    advanced: bool,
    label: Option<String>,
    desc: Option<String>,
    category: Option<String>,
    widget: Option<String>,
    web: Option<String>,
    validate: Option<String>,
    options: Option<String>,
    min: Option<i64>,
    max: Option<i64>,
    step: Option<i64>,
    multiline: bool,
    mono: bool,
}

fn parse_field_attrs(field: &syn::Field, field_name: &str) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    let mut saw_setting = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("setting") {
            continue;
        }
        saw_setting = true;
        attr.parse_nested_meta(|meta| {
            let key = meta
                .path
                .get_ident()
                .map(|i| i.to_string())
                .unwrap_or_default();
            match key.as_str() {
                "skip" => out.skip = true,
                "global_only" => out.global_only = true,
                "advanced" => out.advanced = true,
                "multiline" => out.multiline = true,
                "mono" => out.mono = true,
                "label" => out.label = Some(meta.value()?.parse::<LitStr>()?.value()),
                "desc" => out.desc = Some(meta.value()?.parse::<LitStr>()?.value()),
                "category" => out.category = Some(meta.value()?.parse::<LitStr>()?.value()),
                "widget" => out.widget = Some(meta.value()?.parse::<LitStr>()?.value()),
                "web" => out.web = Some(meta.value()?.parse::<LitStr>()?.value()),
                "validate" => out.validate = Some(meta.value()?.parse::<LitStr>()?.value()),
                "options" => out.options = Some(meta.value()?.parse::<LitStr>()?.value()),
                "min" => out.min = Some(meta.value()?.parse::<LitInt>()?.base10_parse()?),
                "max" => out.max = Some(meta.value()?.parse::<LitInt>()?.base10_parse()?),
                "step" => out.step = Some(meta.value()?.parse::<LitInt>()?.base10_parse()?),
                other => return Err(meta.error(format!("unknown setting key `{other}`"))),
            }
            Ok(())
        })?;
    }
    if !saw_setting && !out.skip {
        return Err(syn::Error::new_spanned(
            field,
            format!("field `{field_name}` needs #[setting(...)] or #[setting(skip)]"),
        ));
    }
    Ok(out)
}

fn build_widget(field: &syn::Field, attrs: &FieldAttrs) -> syn::Result<proc_macro2::TokenStream> {
    let widget = attrs.widget.as_deref().ok_or_else(|| {
        syn::Error::new_spanned(field, "#[setting(widget = \"...\")] is required")
    })?;
    let multiline = attrs.multiline;
    let mono = attrs.mono;
    let ts = match widget {
        "toggle" => quote!(WidgetKind::Toggle),
        "text" => quote!(WidgetKind::Text { multiline: #multiline, mono: #mono }),
        "optional_text" => quote!(WidgetKind::OptionalText { mono: #mono }),
        "list" => quote!(WidgetKind::List),
        "number" => {
            let min = opt_i64(attrs.min);
            let max = opt_i64(attrs.max);
            quote!(WidgetKind::Number { min: #min, max: #max })
        }
        "slider" => {
            let (min, max, step) = (attrs.min, attrs.max, attrs.step);
            let (min, max, step) = (
                min.ok_or_else(|| syn::Error::new_spanned(field, "slider needs min"))?,
                max.ok_or_else(|| syn::Error::new_spanned(field, "slider needs max"))?,
                step.unwrap_or(1),
            );
            if step <= 0 {
                return Err(syn::Error::new_spanned(field, "slider step must be > 0"));
            }
            if min > max {
                return Err(syn::Error::new_spanned(field, "slider min must be <= max"));
            }
            quote!(WidgetKind::Slider { min: #min, max: #max, step: #step })
        }
        "select" => {
            let raw = attrs.options.as_deref().ok_or_else(|| {
                syn::Error::new_spanned(field, "select needs options = \"v:Label,...\"")
            })?;
            let opts = parse_options(field, raw)?;
            quote!(WidgetKind::Select {
                options: ::std::vec![ #(#opts),* ]
            })
        }
        custom if custom.starts_with("custom:") => {
            let id = custom.trim_start_matches("custom:");
            quote!(WidgetKind::Custom { id: #id.to_string() })
        }
        other => {
            return Err(syn::Error::new_spanned(
                field,
                format!("unknown widget `{other}`"),
            ))
        }
    };
    Ok(ts)
}

fn parse_options(field: &syn::Field, raw: &str) -> syn::Result<Vec<proc_macro2::TokenStream>> {
    let mut out = Vec::new();
    for entry in raw.split(',') {
        let (value, label) = entry.split_once(':').ok_or_else(|| {
            syn::Error::new_spanned(field, format!("option `{entry}` must be `value:Label`"))
        })?;
        out.push(quote!(SelectOption::new(#value, #label)));
    }
    Ok(out)
}

fn build_web(field: &syn::Field, attrs: &FieldAttrs) -> syn::Result<proc_macro2::TokenStream> {
    let spec = attrs.web.as_deref().unwrap_or("allow");
    let ts = if spec == "allow" {
        quote!(WebWritePolicy::Allow)
    } else if let Some(reason) = spec.strip_prefix("elevation:") {
        quote!(WebWritePolicy::RequiresElevation { reason: #reason.to_string() })
    } else if let Some(reason) = spec.strip_prefix("local_only:") {
        quote!(WebWritePolicy::LocalOnly { reason: #reason.to_string() })
    } else {
        return Err(syn::Error::new_spanned(
            field,
            format!("unknown web policy `{spec}`"),
        ));
    };
    Ok(ts)
}

fn build_validation(
    field: &syn::Field,
    attrs: &FieldAttrs,
) -> syn::Result<proc_macro2::TokenStream> {
    let spec = attrs.validate.as_deref().unwrap_or("none");
    let ts = match spec {
        "none" => quote!(ValidationKind::None),
        "nonempty" => quote!(ValidationKind::NonEmptyString),
        "memory_limit" => quote!(ValidationKind::MemoryLimit),
        "volume_list" => quote!(ValidationKind::VolumeList),
        "env_list" => quote!(ValidationKind::EnvList),
        "port_mapping_list" => quote!(ValidationKind::PortMappingList),
        range if range.starts_with("range:") => {
            let parts: Vec<&str> = range.trim_start_matches("range:").split(':').collect();
            if parts.is_empty() || parts.len() > 2 {
                return Err(syn::Error::new_spanned(
                    field,
                    "range must be `range:min` or `range:min:max`",
                ));
            }
            let min: u64 = parts
                .first()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| syn::Error::new_spanned(field, "range needs min"))?;
            let max = match parts.get(1) {
                Some(s) => {
                    let m: u64 = s
                        .parse()
                        .map_err(|_| syn::Error::new_spanned(field, "range max not a u64"))?;
                    quote!(::std::option::Option::Some(#m))
                }
                None => quote!(::std::option::Option::None),
            };
            quote!(ValidationKind::RangeU64 { min: #min, max: #max })
        }
        other => {
            return Err(syn::Error::new_spanned(
                field,
                format!("unknown validate rule `{other}`"),
            ))
        }
    };
    Ok(ts)
}

fn opt_i64(v: Option<i64>) -> proc_macro2::TokenStream {
    match v {
        Some(n) => quote!(::std::option::Option::Some(#n)),
        None => quote!(::std::option::Option::None),
    }
}

/// Collect `#[doc = "..."]` lines into a single trimmed description string.
fn doc_comment(field: &syn::Field) -> Option<String> {
    let mut lines = Vec::new();
    for attr in &field.attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let syn::Meta::NameValue(nv) = &attr.meta {
            if let syn::Expr::Lit(expr) = &nv.value {
                if let syn::Lit::Str(s) = &expr.lit {
                    lines.push(s.value().trim().to_string());
                }
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join(" ").trim().to_string())
    }
}

/// "max_concurrent_workers" -> "Max concurrent workers".
fn humanize(field_name: &str) -> String {
    let spaced = field_name.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}
