//! `#[derive(Entity)]` for rainier_orm.
//!
//! From a plain struct it generates the `Entity` impl: table + column
//! metadata (column SQL type is deferred to the field type's `SqlType`
//! impl, so it stays open to new types), a row decoder (`from_row`, via
//! each field type's `FromColumn`), the insert/primary-key value projections
//! (via `ToColumn`), plus index and foreign-key constraint metadata. No
//! per-entity SQL is written by hand.
//!
//! ```ignore
//! #[derive(Entity)]
//! #[orm(table = "posts")]
//! #[orm(index = "author_id, created_at")]      // composite index
//! #[orm(unique = "author_id, slug")]           // composite unique
//! struct Post {
//!     #[orm(pk, auto_increment)]
//!     id: u64,
//!     #[orm(unique)]                            // single-column unique
//!     slug: String,
//!     #[orm(index, references = "users(id)", on_delete = "cascade")]
//!     author_id: u64,                            // FK *id*, not a relationship
//!     body: Option<String>,                      // nullable inferred from Option
//!     #[orm(column = "created_at")]
//!     created: chrono::DateTime<chrono::Utc>,
//! }
//! ```
//!
//! ## Composite primary keys
//!
//! Mark more than one field `#[orm(pk)]` and the key is all of them, in
//! declaration order — which is the order they appear in `PRIMARY KEY (a, b)`,
//! so it decides which prefix lookups the index can serve and is worth choosing
//! deliberately:
//!
//! ```ignore
//! #[derive(Entity)]
//! #[orm(table = "memberships")]
//! struct Membership {
//!     #[orm(pk)]
//!     team_id: u64,
//!     #[orm(pk)]
//!     user_id: u64,
//!     role: String,
//! }
//! ```
//!
//! The difference this makes to generated code is confined to the key: no key
//! column appears in `update_values()`, `WHERE` clauses `AND` every part
//! together, and the `CREATE TABLE` gets one table-level constraint instead of
//! an inline `PRIMARY KEY` per column (two of which no engine accepts).
//!
//! A single-key struct additionally gets an `impl SingleKey`, which is what lets
//! it be passed to the APIs taking one key value (`find_by_pk`, `delete_by_pk`,
//! `cursor`). A composite struct does not, so those calls fail to compile rather
//! than building a `WHERE` from the first column alone.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, LitStr};

#[proc_macro_derive(Entity, attributes(orm))]
pub fn derive_entity(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = input.ident.clone();

    struct IndexDef {
        columns: Vec<String>,
        unique: bool,
    }

    // --- struct-level: table name + composite index/unique declarations ---
    let mut table = to_snake_case(&ident.to_string());
    let mut composite: Vec<IndexDef> = Vec::new();
    for attr in &input.attrs {
        if attr.path().is_ident("orm") {
            let _ = attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("table") {
                    table = meta.value()?.parse::<LitStr>()?.value();
                } else if meta.path.is_ident("index") {
                    composite.push(IndexDef {
                        columns: split_columns(&meta.value()?.parse::<LitStr>()?.value()),
                        unique: false,
                    });
                } else if meta.path.is_ident("unique") {
                    composite.push(IndexDef {
                        columns: split_columns(&meta.value()?.parse::<LitStr>()?.value()),
                        unique: true,
                    });
                }
                Ok(())
            });
        }
    }

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(n) => n.named.clone(),
            _ => return compile_err("Entity requires named struct fields"),
        },
        _ => return compile_err("Entity can only be derived for a struct"),
    };

    struct FieldMeta {
        ident: syn::Ident,
        ty: syn::Type,
        column: String,
        pk: bool,
        auto_increment: bool,
        unique: bool,
        index: bool,
        shard: bool,
        references: Option<(String, String)>, // (foreign_table, foreign_column)
        on_delete: Option<String>,
        on_update: Option<String>,
    }

    let mut metas: Vec<FieldMeta> = Vec::new();
    // Every `#[orm(pk)]` field, in declaration order. Order is load-bearing for
    // a composite key: it fixes the column order of `PRIMARY KEY (a, b)` (and so
    // which prefix lookups the index serves) and the positional order key values
    // are supplied in.
    let mut pks: Vec<(syn::Ident, String)> = Vec::new();
    for f in &fields {
        let fident = f.ident.clone().unwrap();
        // `to_string()` on a raw identifier keeps the `r#`, so a field written
        // `pub r#type` would become the column `r#type` and every query naming
        // it would fail with "no such column". A column called `type`, `match`
        // or `ref` is unremarkable in a database and a keyword in Rust, so the
        // raw form is the only way to spell it — the derive has to understand
        // that rather than pass it through.
        let mut column = fident.to_string();
        if let Some(stripped) = column.strip_prefix("r#") {
            column = stripped.to_string();
        }
        let mut is_pk = false;
        let mut auto_increment = false;
        let mut unique = false;
        let mut index = false;
        let mut shard = false;
        let mut references = None;
        let mut on_delete = None;
        let mut on_update = None;
        for attr in &f.attrs {
            if attr.path().is_ident("orm") {
                let _ = attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("pk") {
                        is_pk = true;
                    } else if meta.path.is_ident("auto_increment") {
                        auto_increment = true;
                    } else if meta.path.is_ident("unique") {
                        unique = true;
                    } else if meta.path.is_ident("index") {
                        index = true;
                    } else if meta.path.is_ident("shard_key") {
                        shard = true;
                    } else if meta.path.is_ident("column") {
                        column = meta.value()?.parse::<LitStr>()?.value();
                    } else if meta.path.is_ident("references") {
                        let v = meta.value()?.parse::<LitStr>()?.value();
                        references = Some(parse_references(&v));
                    } else if meta.path.is_ident("on_delete") {
                        on_delete = Some(meta.value()?.parse::<LitStr>()?.value());
                    } else if meta.path.is_ident("on_update") {
                        on_update = Some(meta.value()?.parse::<LitStr>()?.value());
                    }
                    Ok(())
                });
            }
        }
        if is_pk {
            pks.push((fident.clone(), column.clone()));
        }
        metas.push(FieldMeta {
            ident: fident,
            ty: f.ty.clone(),
            column,
            pk: is_pk,
            auto_increment,
            unique,
            index,
            shard,
            references,
            on_delete,
            on_update,
        });
    }

    // A struct with no key stays an error. A table the ORM cannot identify a row
    // in is nearly always a forgotten attribute rather than an intent, and the
    // consequence of guessing wrong is an `UPDATE` with an empty `WHERE`.
    if pks.is_empty() {
        return compile_err("Entity needs at least one field marked `#[orm(pk)]`");
    }
    let pk_cols: Vec<&String> = pks.iter().map(|(_, column)| column).collect();
    let pk_idents: Vec<&syn::Ident> = pks.iter().map(|(ident, _)| ident).collect();
    // The first key part backs `primary_key()`/`pk_value()`, which name the key
    // for routing and binding rather than build a predicate from it.
    let (pk_ident, pk_col) = (pk_idents[0], pk_cols[0]);

    // `SingleKey` is what makes `find_by_pk`/`delete_by_pk`/`cursor` — the APIs
    // that take one key value — reject a composite entity at compile time, so it
    // is emitted only when there is genuinely one key column.
    let single_key_impl = if pks.len() == 1 {
        quote! { impl ::rainier_orm::SingleKey for #ident {} }
    } else {
        quote! {}
    };

    let col_specs = metas.iter().map(|m| {
        let name = &m.column;
        let ty = &m.ty;
        let is_pk = m.pk;
        let ai = m.auto_increment;
        let uniq = m.unique;
        quote! {
            ::rainier_orm::ColumnSpec {
                name: #name,
                ty: <#ty as ::rainier_orm::SqlType>::COLUMN_TYPE,
                nullable: <#ty as ::rainier_orm::SqlType>::NULLABLE,
                pk: #is_pk,
                auto_increment: #ai,
                unique: #uniq,
            }
        }
    });

    // --- indexes(): single-column `#[orm(index)]` + struct composites ---
    let mut index_specs: Vec<TokenStream2> = Vec::new();
    for m in &metas {
        if m.index {
            let name = format!("idx_{table}_{}", m.column);
            let col = &m.column;
            index_specs.push(quote! {
                ::rainier_orm::IndexSpec { name: #name, columns: &[#col], unique: false }
            });
        }
    }
    for ix in &composite {
        let joined = ix.columns.join("_");
        let prefix = if ix.unique { "uq" } else { "idx" };
        let name = format!("{prefix}_{table}_{joined}");
        let cols = &ix.columns;
        let uniq = ix.unique;
        index_specs.push(quote! {
            ::rainier_orm::IndexSpec { name: #name, columns: &[#(#cols),*], unique: #uniq }
        });
    }

    // --- foreign_keys(): one per `#[orm(references = "...")]` field ---
    let fk_specs = metas.iter().filter_map(|m| {
        let (ftable, fcol) = m.references.as_ref()?;
        let name = format!("fk_{table}_{}", m.column);
        let col = &m.column;
        let on_delete = ref_action(m.on_delete.as_deref());
        let on_update = ref_action(m.on_update.as_deref());
        Some(quote! {
            ::rainier_orm::ForeignKeySpec {
                name: #name,
                columns: &[#col],
                foreign_table: #ftable,
                foreign_columns: &[#fcol],
                on_delete: #on_delete,
                on_update: #on_update,
            }
        })
    });

    // Shard-encoded columns (`#[orm(shard)]`) — routing keys.
    let shard_cols = metas.iter().filter(|m| m.shard).map(|m| {
        let c = &m.column;
        quote! { #c }
    });

    let from_row_fields = metas.iter().map(|m| {
        let fident = &m.ident;
        let ty = &m.ty;
        let col = &m.column;
        quote! { #fident: <#ty as ::rainier_orm::FromColumn>::from_column(row, #col)? }
    });

    // Inserts skip auto-increment primary keys (let the DB assign them).
    let insert_vals = metas.iter().filter(|m| !m.auto_increment).map(|m| {
        let fident = &m.ident;
        let col = &m.column;
        quote! { (#col, ::rainier_orm::ToColumn::to_value(&self.#fident)) }
    });
    // Updates set every non-primary-key column — for a composite key that means
    // every part of it stays out of the `SET`, so a save can never move a row to
    // a different key.
    let update_vals = metas.iter().filter(|m| !m.pk).map(|m| {
        let fident = &m.ident;
        let col = &m.column;
        quote! { (#col, ::rainier_orm::ToColumn::to_value(&self.#fident)) }
    });

    // Key values, positionally matching `primary_key_columns()`.
    let pk_vals = pk_idents.iter().map(|fident| {
        quote! { ::rainier_orm::ToColumn::to_value(&self.#fident) }
    });

    quote! {
        impl ::rainier_orm::Entity for #ident {
            fn table() -> &'static str { #table }
            fn columns() -> &'static [::rainier_orm::ColumnSpec] {
                &[ #(#col_specs),* ]
            }
            fn primary_key_columns() -> &'static [&'static str] {
                &[ #(#pk_cols),* ]
            }
            fn primary_key() -> &'static str { #pk_col }
            fn indexes() -> &'static [::rainier_orm::IndexSpec] {
                &[ #(#index_specs),* ]
            }
            fn foreign_keys() -> &'static [::rainier_orm::ForeignKeySpec] {
                &[ #(#fk_specs),* ]
            }
            fn shard_columns() -> &'static [&'static str] {
                &[ #(#shard_cols),* ]
            }
            fn from_row(row: &dyn ::rainier_orm::Row)
                -> ::core::result::Result<Self, ::rainier_orm::Error>
            {
                ::core::result::Result::Ok(Self { #(#from_row_fields),* })
            }
            fn insert_values(&self)
                -> ::std::vec::Vec<(&'static str, ::rainier_orm::sea_query::Value)>
            {
                ::std::vec![ #(#insert_vals),* ]
            }
            fn update_values(&self)
                -> ::std::vec::Vec<(&'static str, ::rainier_orm::sea_query::Value)>
            {
                ::std::vec![ #(#update_vals),* ]
            }
            fn pk_values(&self) -> ::std::vec::Vec<::rainier_orm::sea_query::Value> {
                ::std::vec![ #(#pk_vals),* ]
            }
            fn pk_value(&self) -> ::rainier_orm::sea_query::Value {
                ::rainier_orm::ToColumn::to_value(&self.#pk_ident)
            }
        }

        #single_key_impl
    }
    .into()
}

/// `"a, b , c"` → `["a", "b", "c"]`.
fn split_columns(s: &str) -> Vec<String> {
    s.split(',').map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect()
}

/// Parse `references` targets: `"users(id)"` or `"users.id"` → `("users","id")`.
/// A bare `"users"` defaults the column to `id`.
fn parse_references(s: &str) -> (String, String) {
    let s = s.trim();
    if let Some(open) = s.find('(') {
        let table = s[..open].trim().to_string();
        let col = s[open + 1..].trim_end_matches([')', ' ']).trim().to_string();
        (table, col)
    } else if let Some(dot) = s.find('.') {
        (s[..dot].trim().to_string(), s[dot + 1..].trim().to_string())
    } else {
        (s.to_string(), "id".to_string())
    }
}

/// Map an `on_delete`/`on_update` string to an `Option<RefAction>` token
/// stream (`None` when unset; an unknown action becomes `None` and the DB
/// default applies).
fn ref_action(s: Option<&str>) -> TokenStream2 {
    let Some(s) = s else {
        return quote! { ::core::option::Option::None };
    };
    let variant = match s.trim().to_ascii_lowercase().replace([' ', '-'], "_").as_str() {
        "cascade" => quote! { Cascade },
        "restrict" => quote! { Restrict },
        "set_null" | "setnull" => quote! { SetNull },
        "set_default" | "setdefault" => quote! { SetDefault },
        "no_action" | "noaction" => quote! { NoAction },
        _ => return quote! { ::core::option::Option::None },
    };
    quote! { ::core::option::Option::Some(::rainier_orm::RefAction::#variant) }
}

fn to_snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.extend(ch.to_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn compile_err(msg: &str) -> TokenStream {
    quote! { compile_error!(#msg); }.into()
}

/// Derive a [test factory] for an entity.
///
/// ```ignore
/// #[derive(Entity, Clone, Default, Factory)]
/// struct User { id: u64, email: String }
///
/// let users = User::factory().count(3).make();
/// ```
///
/// Generates `User::factory()`, building each row from [`Default`] — so the
/// type needs one, and three rows are three identical rows until a
/// `.sequence(..)` distinguishes them. Anything with a `UNIQUE` column
/// therefore needs one; see the trait's own documentation for why that is not
/// automatic.
///
/// The expansion is deliberately tiny. A derive that invented plausible values
/// per field would have to guess what each column means, and a wrong guess
/// produces a row that fails to insert for a reason nobody can see from the
/// test that wrote it.
///
/// [test factory]: https://docs.rs/rainier-database/latest/rainier_database/factory/index.html
#[proc_macro_derive(Factory)]
pub fn derive_factory(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = input.ident;
    let (impl_generics, type_generics, where_clause) = input.generics.split_for_impl();

    quote! {
        impl #impl_generics ::rainier_database::factory::HasFactory for #ident #type_generics
        #where_clause
        {
            fn factory() -> ::rainier_database::factory::Factory<Self> {
                ::rainier_database::factory::Factory::new(|_index| <Self as Default>::default())
            }
        }
    }
    .into()
}
