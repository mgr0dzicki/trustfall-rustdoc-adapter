use std::{
    borrow::Borrow,
    collections::{BTreeSet, HashMap, HashSet},
};

use rustdoc_types::{Crate, GenericArgs, Id, Item, ItemEnum, Typedef, Visibility};

/// The rustdoc for a crate, together with associated indexed data to speed up common operations.
///
/// Besides the parsed rustdoc, it also contains some manually-inlined `rustdoc_types::Trait`s
/// of the most common built-in traits.
/// This is a temporary step, until we're able to combine rustdocs of multiple crates.
#[derive(Debug, Clone)]
pub struct IndexedCrate<'a> {
    pub(crate) inner: &'a Crate,

    /// For an Id, give the list of item Ids under which it is publicly visible.
    pub(crate) visibility_forest: HashMap<&'a Id, Vec<&'a Id>>,

    /// index: importable name (in any namespace) -> list of items under that name
    pub(crate) imports_index: Option<HashMap<ImportablePath<'a>, Vec<&'a Item>>>,

    /// index: impl owner + impl'd item name -> list of (impl itself, the named item))
    pub(crate) impl_index: Option<HashMap<ImplEntry<'a>, Vec<(&'a Item, &'a Item)>>>,

    /// Trait items defined in external crates are not present in the `inner: &Crate` field,
    /// even if they are implemented by a type in that crate. This also includes
    /// Rust's built-in traits like `Debug, Send, Eq` etc.
    ///
    /// This change is approximately as of rustdoc v23,
    /// in <https://github.com/rust-lang/rust/pull/105182>
    ///
    /// As a temporary workaround, we manually create the trait items
    /// for the most common Rust built-in traits and link to those items
    /// as if they were still part of the rustdoc JSON file.
    ///
    /// A more complete future solution may generate multiple crates' rustdoc JSON
    /// and link to the external crate's trait items as necessary.
    pub(crate) manually_inlined_builtin_traits: HashMap<Id, Item>,
}

impl<'a> IndexedCrate<'a> {
    pub fn new(crate_: &'a Crate) -> Self {
        let mut value = Self {
            inner: crate_,
            visibility_forest: compute_parent_ids_for_public_items(crate_)
                .into_iter()
                .map(|(key, values)| {
                    // Ensure a consistent order, since queries can observe this order directly.
                    let mut values: Vec<_> = values.into_iter().collect();
                    values.sort_unstable_by_key(|x| &x.0);
                    (key, values)
                })
                .collect(),
            manually_inlined_builtin_traits: create_manually_inlined_builtin_traits(crate_),
            imports_index: None,
            impl_index: None,
        };

        let mut imports_index: HashMap<ImportablePath, Vec<&Item>> =
            HashMap::with_capacity(crate_.index.len());
        for item in crate_.index.values().filter_map(|item| {
            matches!(
                item.inner,
                rustdoc_types::ItemEnum::Struct(..)
                    | rustdoc_types::ItemEnum::StructField(..)
                    | rustdoc_types::ItemEnum::Enum(..)
                    | rustdoc_types::ItemEnum::Variant(..)
                    | rustdoc_types::ItemEnum::Function(..)
                    | rustdoc_types::ItemEnum::Impl(..)
                    | rustdoc_types::ItemEnum::Trait(..)
            )
            .then_some(item)
        }) {
            for importable_path in value.publicly_importable_names(&item.id) {
                imports_index
                    .entry(ImportablePath::new(importable_path))
                    .or_default()
                    .push(item);
            }
        }
        let index_size = imports_index.len();
        value.imports_index = Some(imports_index);

        let mut impl_index: HashMap<ImplEntry<'a>, Vec<(&'a Item, &'a Item)>> =
            HashMap::with_capacity(index_size);
        for (id, impl_items) in crate_.index.iter().filter_map(|(id, item)| {
            let impls = match &item.inner {
                rustdoc_types::ItemEnum::Struct(s) => &s.impls,
                rustdoc_types::ItemEnum::Enum(e) => &e.impls,
                rustdoc_types::ItemEnum::Union(u) => &u.impls,
                _ => return None,
            };

            let impl_items = impls.iter().filter_map(|impl_id| crate_.index.get(impl_id));

            Some((id, impl_items))
        }) {
            for impl_item in impl_items {
                let impl_inner = match &impl_item.inner {
                    rustdoc_types::ItemEnum::Impl(impl_inner) => impl_inner,
                    _ => unreachable!("expected impl but got another item type: {impl_item:?}"),
                };
                let trait_provided_methods: BTreeSet<_> = impl_inner
                    .provided_trait_methods
                    .iter()
                    .map(|x| x.as_str())
                    .collect();
                if let Some(trait_item) = impl_inner
                    .trait_
                    .as_ref()
                    .and_then(|trait_path| crate_.index.get(&trait_path.id))
                {
                    if let rustdoc_types::ItemEnum::Trait(trait_item) = &trait_item.inner {
                        for provided_item in trait_item
                            .items
                            .iter()
                            .filter_map(|id| crate_.index.get(id))
                            .filter(|item| {
                                item.name
                                    .as_deref()
                                    .map(|name| trait_provided_methods.contains(name))
                                    .unwrap_or_default()
                            })
                        {
                            impl_index
                                .entry(ImplEntry::new(
                                    id,
                                    provided_item
                                        .name
                                        .as_deref()
                                        .expect("item should have had a name"),
                                ))
                                .or_default()
                                .push((impl_item, provided_item));
                        }
                    }
                }

                for contained_item in impl_inner
                    .items
                    .iter()
                    .filter_map(|item_id| crate_.index.get(item_id))
                {
                    if let Some(contained_item_name) = contained_item.name.as_deref() {
                        impl_index
                            .entry(ImplEntry::new(id, contained_item_name))
                            .or_default()
                            .push((impl_item, contained_item));
                    }
                }
            }
        }
        value.impl_index = Some(impl_index);

        value
    }

    /// Return all the paths (as Vec<&'a str> of component names, joinable with "::")
    /// with which the given item can be imported from this crate.
    pub fn publicly_importable_names(&self, id: &'a Id) -> Vec<Vec<&'a str>> {
        let mut result = vec![];

        if self.inner.index.contains_key(id) {
            let mut already_visited_ids = Default::default();
            self.collect_publicly_importable_names(
                id,
                &mut already_visited_ids,
                &mut vec![],
                &mut result,
            );
        }

        result
    }

    fn collect_publicly_importable_names(
        &self,
        next_id: &'a Id,
        already_visited_ids: &mut HashSet<&'a Id>,
        stack: &mut Vec<&'a str>,
        output: &mut Vec<Vec<&'a str>>,
    ) {
        if !already_visited_ids.insert(next_id) {
            // We found a cycle, and we've already processed this item.
            // Nothing more to do here.
            return;
        }

        let item = &self.inner.index[next_id];
        if !stack.is_empty()
            && matches!(
                item.inner,
                ItemEnum::Impl(..) | ItemEnum::Struct(..) | ItemEnum::Union(..)
            )
        {
            // Structs, unions, and impl blocks are not modules.
            // They *themselves* can be imported, but the items they contain cannot be imported.
            // Since the stack is non-empty, we must be trying to determine importable names
            // for a descendant item of a struct / union / impl. There are none.
            //
            // We explicitly do *not* want to check for Enum here,
            // since enum variants *are* importable.
            return;
        }

        let (push_name, popped_name) = match &item.inner {
            rustdoc_types::ItemEnum::Import(import_item) => {
                if import_item.glob {
                    // Glob imports refer to the *contents* of the named item, not the item itself.
                    // Rust doesn't allow glob imports to rename items, so there's no name to add.
                    (None, None)
                } else {
                    // Use the name of the imported item, since it might be renaming
                    // the item being imported.
                    let push_name = Some(import_item.name.as_str());

                    // The imported item may be renamed here, so pop it from the stack.
                    let popped_name = Some(stack.pop().expect("no name to pop"));

                    (push_name, popped_name)
                }
            }
            rustdoc_types::ItemEnum::Typedef(..) => {
                // Use the typedef name instead of the underlying item's own name,
                // since it might be renaming the underlying item.
                let push_name = Some(item.name.as_deref().expect("typedef had no name"));

                // If there is an underlying item, pop it from the stack
                // since it may be renamed here.
                let popped_name = stack.pop();

                (push_name, popped_name)
            }
            _ => (item.name.as_deref(), None),
        };

        // Push the new name onto the stack, if there is one.
        if let Some(pushed_name) = push_name {
            stack.push(pushed_name);
        }

        self.collect_publicly_importable_names_inner(next_id, already_visited_ids, stack, output);

        // Undo any changes made to the stack, returning it to its pre-recursion state.
        if let Some(pushed_name) = push_name {
            let recovered_name = stack.pop().expect("there was nothing to pop");
            assert_eq!(pushed_name, recovered_name);
        }
        if let Some(popped_name) = popped_name {
            stack.push(popped_name);
        }

        // We're leaving this item. Remove it from the visited set.
        let removed = already_visited_ids.remove(next_id);
        assert!(removed);
    }

    fn collect_publicly_importable_names_inner(
        &self,
        next_id: &'a Id,
        already_visited_ids: &mut HashSet<&'a Id>,
        stack: &mut Vec<&'a str>,
        output: &mut Vec<Vec<&'a str>>,
    ) {
        if next_id == &self.inner.root {
            let final_name = stack.iter().rev().copied().collect();
            output.push(final_name);
        } else if let Some(visible_parents) = self.visibility_forest.get(next_id) {
            for parent_id in visible_parents.iter().copied() {
                self.collect_publicly_importable_names(
                    parent_id,
                    already_visited_ids,
                    stack,
                    output,
                );
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ImportablePath<'a> {
    pub(crate) components: Vec<&'a str>,
}

impl<'a> ImportablePath<'a> {
    fn new(components: Vec<&'a str>) -> Self {
        Self { components }
    }
}

impl<'a: 'b, 'b> Borrow<[&'b str]> for ImportablePath<'a> {
    fn borrow(&self) -> &[&'b str] {
        &self.components
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ImplEntry<'a> {
    /// Tuple of:
    /// - the Id of the struct/enum/union that owns the item,
    /// - the name of the item in the owner's `impl` block.
    ///
    /// Stored as a tuple to make the `Borrow` impl work.
    pub(crate) data: (&'a Id, &'a str),
}

impl<'a> ImplEntry<'a> {
    #[inline]
    fn new(owner_id: &'a Id, item_name: &'a str) -> Self {
        Self {
            data: (owner_id, item_name),
        }
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn owner_id(&self) -> &'a Id {
        self.data.0
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn item_name(&self) -> &'a str {
        self.data.1
    }
}

impl<'a: 'b, 'b> Borrow<(&'b Id, &'b str)> for ImplEntry<'a> {
    fn borrow(&self) -> &(&'b Id, &'b str) {
        &(self.data)
    }
}

fn compute_parent_ids_for_public_items(crate_: &Crate) -> HashMap<&Id, HashSet<&Id>> {
    let mut result = Default::default();
    let root_id = &crate_.root;
    if let Some(root_module) = crate_.index.get(root_id) {
        if root_module.visibility == Visibility::Public {
            let mut currently_visited_items = Default::default();
            visit_root_reachable_public_items(
                crate_,
                &mut result,
                &mut currently_visited_items,
                root_module,
                None,
            );
        }
    }

    result
}

/// Collect all public items that are reachable from the crate root and record their parent Ids.
fn visit_root_reachable_public_items<'a>(
    crate_: &'a Crate,
    parents: &mut HashMap<&'a Id, HashSet<&'a Id>>,
    currently_visited_items: &mut HashSet<&'a Id>,
    item: &'a Item,
    parent_id: Option<&'a Id>,
) {
    match item.visibility {
        Visibility::Crate => {
            if matches!(item.inner, ItemEnum::Impl(_)) {
                // A bug in rustdoc of Rust 1.69 and older causes `impl` items
                // to be given `crate` visibility instead of the correct `default` visibility.
                // Rust does not support `pub(crate) impl` or other visibility modifiers,
                // so if we're in this block, we're affected by the bug.
                //
                // The fix has shipped in 1.70 beta, but that still uses rustdoc v24.
                // TODO: Remove this in rustdoc v25+ since the fix should be present there.
            } else {
                // This item is not public, so we don't need to process it.
                return;
            }
        }
        Visibility::Restricted { .. } => {
            // This item is not public, so we don't need to process it.
            return;
        }
        Visibility::Public => {} // Public item, keep going.
        Visibility::Default => {
            // Enum variants, and some impls and methods have default visibility:
            // they are visible only if the type to which they belong is visible.
            // However, we don't recurse into non-public items with this function, so
            // reachable items with default visibility must be public.
        }
    }

    let item_parents = parents.entry(&item.id).or_default();
    if let Some(parent_id) = parent_id {
        item_parents.insert(parent_id);
    }

    if !currently_visited_items.insert(&item.id) {
        // We found a cycle in the import graph, and we've already processed this item.
        // Nothing more to do here.
        return;
    }

    let next_parent_id = Some(&item.id);
    match &item.inner {
        rustdoc_types::ItemEnum::Module(m) => {
            for inner in m.items.iter().filter_map(|id| crate_.index.get(id)) {
                visit_root_reachable_public_items(
                    crate_,
                    parents,
                    currently_visited_items,
                    inner,
                    next_parent_id,
                );
            }
        }
        rustdoc_types::ItemEnum::Import(imp) => {
            // Imports of modules, and glob imports of enums,
            // import the *contents* of the pointed-to item rather than the item itself.
            if let Some(imported_item) = imp.id.as_ref().and_then(|id| crate_.index.get(id)) {
                if imp.glob {
                    // Glob imports point directly to the contents of the pointed-to module.
                    // For each item in that module, the import's parent becomes its parent as well.
                    let next_parent_id = parent_id;

                    let inner_ids = match &imported_item.inner {
                        rustdoc_types::ItemEnum::Module(mod_item) => &mod_item.items,
                        rustdoc_types::ItemEnum::Enum(enum_item) => &enum_item.variants,
                        _ => unreachable!(
                            "found a glob import of an unexpected kind of item: \
                            {imp:?} {imported_item:?}"
                        ),
                    };
                    for inner_id in inner_ids {
                        if let Some(item) = crate_.index.get(inner_id) {
                            visit_root_reachable_public_items(
                                crate_,
                                parents,
                                currently_visited_items,
                                item,
                                next_parent_id,
                            );
                        }
                    }
                } else {
                    visit_root_reachable_public_items(
                        crate_,
                        parents,
                        currently_visited_items,
                        imported_item,
                        next_parent_id,
                    );
                }
            }
        }
        rustdoc_types::ItemEnum::Struct(struct_) => {
            let field_ids_iter: Box<dyn Iterator<Item = &Id>> = match &struct_.kind {
                rustdoc_types::StructKind::Unit => Box::new(std::iter::empty()),
                rustdoc_types::StructKind::Tuple(field_ids) => {
                    Box::new(field_ids.iter().filter_map(|x| x.as_ref()))
                }
                rustdoc_types::StructKind::Plain { fields, .. } => Box::new(fields.iter()),
            };

            for inner in field_ids_iter
                .chain(struct_.impls.iter())
                .filter_map(|id| crate_.index.get(id))
            {
                visit_root_reachable_public_items(
                    crate_,
                    parents,
                    currently_visited_items,
                    inner,
                    next_parent_id,
                );
            }
        }
        rustdoc_types::ItemEnum::Enum(enum_) => {
            for inner in enum_
                .variants
                .iter()
                .chain(enum_.impls.iter())
                .filter_map(|id| crate_.index.get(id))
            {
                visit_root_reachable_public_items(
                    crate_,
                    parents,
                    currently_visited_items,
                    inner,
                    next_parent_id,
                );
            }
        }
        rustdoc_types::ItemEnum::Union(union_) => {
            for inner in union_
                .fields
                .iter()
                .chain(union_.impls.iter())
                .filter_map(|id| crate_.index.get(id))
            {
                visit_root_reachable_public_items(
                    crate_,
                    parents,
                    currently_visited_items,
                    inner,
                    next_parent_id,
                );
            }
        }
        rustdoc_types::ItemEnum::Trait(trait_) => {
            for inner in trait_.items.iter().filter_map(|id| crate_.index.get(id)) {
                visit_root_reachable_public_items(
                    crate_,
                    parents,
                    currently_visited_items,
                    inner,
                    next_parent_id,
                );
            }
        }
        rustdoc_types::ItemEnum::Impl(impl_) => {
            for inner in impl_.items.iter().filter_map(|id| crate_.index.get(id)) {
                visit_root_reachable_public_items(
                    crate_,
                    parents,
                    currently_visited_items,
                    inner,
                    next_parent_id,
                );
            }
        }
        rustdoc_types::ItemEnum::Typedef(ty) => {
            // We're interested in type aliases that are specifically used to rename types:
            //   `pub type Foo = Bar`
            // If the underlying type is generic, it's only a valid renaming if the typedef
            // is also generic in all the same parameters.
            //
            // The Rust compiler ignores `where` bounds on typedefs, so we ignore them too.
            if let Some(reexport_target) = get_typedef_equivalent_reexport_target(crate_, ty) {
                visit_root_reachable_public_items(
                    crate_,
                    parents,
                    currently_visited_items,
                    reexport_target,
                    next_parent_id,
                );
            }
        }
        _ => {
            // No-op, no further items within to consider.
        }
    }

    // We are leaving this item. Remove it from the visited set.
    let removed = currently_visited_items.remove(&item.id);
    assert!(removed);
}

/// Type aliases can sometimes be equivalent to a regular `pub use` re-export:
/// `pub type Foo = crate::Bar` is an example, equivalent to `pub use crate::Bar`.
///
/// If the underlying type has generic parameters, the type alias must include
/// all the same generic parameters in the same order.
/// `pub type Foo<A, B> = crate::Bar<B, A>` is *not* equivalent to `pub use crate::Bar`.
///
/// If the underlying type has default values for any of its generic parameters,
/// the same exact parameters with the same order and defaults must be present on the type alias.
/// `pub type Foo<A> = crate::Bar<A>` is *not* equivalent to `crate::Bar<A, B = ()>`
/// since `Foo<A, B = i64>` is not valid whereas `crate::Bar<A, B = i64>` is fine.
fn get_typedef_equivalent_reexport_target<'a>(
    crate_: &'a Crate,
    ty: &'a Typedef,
) -> Option<&'a Item> {
    if let rustdoc_types::Type::ResolvedPath(resolved_path) = &ty.type_ {
        let underlying = crate_.index.get(&resolved_path.id)?;

        if let Some(GenericArgs::AngleBracketed { args, bindings }) = resolved_path.args.as_deref()
        {
            if !bindings.is_empty() {
                // The type alias specifies some of the underlying type's generic parameters.
                // This is not equivalent to a re-export.
                return None;
            }

            let underlying_generics = match &underlying.inner {
                rustdoc_types::ItemEnum::Struct(struct_) => &struct_.generics,
                rustdoc_types::ItemEnum::Enum(enum_) => &enum_.generics,
                rustdoc_types::ItemEnum::Trait(trait_) => &trait_.generics,
                rustdoc_types::ItemEnum::Union(union_) => &union_.generics,
                rustdoc_types::ItemEnum::Typedef(ty) => &ty.generics,
                _ => unreachable!("unexpected underlying item kind: {underlying:?}"),
            };

            // For the typedef to be equivalent to a re-export, all of the following must hold:
            // - The typedef has the same number of generic parameters as the underlying.
            // - All underlying generic parameters are available on the typedef,
            //   are of the same kind, in the same order, with the same defaults.
            if ty.generics.params.len() != args.len() {
                // The typedef takes a different number of parameters than
                // it supplies to the underlying type. It cannot be a re-export.
                return None;
            }
            if underlying_generics.params.len() != args.len() {
                // The underlying type supports more generic parameter than the typedef supplies
                // when using it -- the unspecified generic parameters take the default values
                // that must have been specified on the underlying type.
                // Nevertheless, this is not a re-export since the types are not equivalent.
                return None;
            }
            for (ty_generic, (underlying_param, arg_generic)) in ty
                .generics
                .params
                .iter()
                .zip(underlying_generics.params.iter().zip(args.iter()))
            {
                let arg_generic_name = match arg_generic {
                    rustdoc_types::GenericArg::Lifetime(name) => name.as_str(),
                    rustdoc_types::GenericArg::Type(rustdoc_types::Type::Generic(t)) => t.as_str(),
                    rustdoc_types::GenericArg::Type(_) => return None,
                    rustdoc_types::GenericArg::Const(c) => {
                        // Nominally, this is the const expression, not the const generic's name.
                        // However, except for pathological edge cases, if the expression is not
                        // simply the const generic parameter itself, then the type isn't the same.
                        //
                        // An example pathological case where this isn't the case is:
                        // `pub type Foo<const N: usize> = Underlying<N + 1 - 1>;`
                        // Detecting that this is the same expression requires that one of
                        // rustdoc or our code do const-evaluation here.
                        //
                        // Const expressions like this are currently only on nightly,
                        // so we can't test them on stable Rust at the moment.
                        //
                        // TODO: revisit this decision when const expressions in types are stable
                        c.expr.as_str()
                    }
                    rustdoc_types::GenericArg::Infer => return None,
                };
                if ty_generic.name.as_str() != arg_generic_name {
                    // The typedef params are not in the same order as the underlying type's.
                    return None;
                }

                match (&ty_generic.kind, &underlying_param.kind) {
                    (
                        rustdoc_types::GenericParamDefKind::Lifetime { .. },
                        rustdoc_types::GenericParamDefKind::Lifetime { .. },
                    ) => {
                        // Typedefs cannot have "outlives" relationships on their lifetimes,
                        // so there's nothing further to compare here. So far, it's a match.
                    }
                    (
                        rustdoc_types::GenericParamDefKind::Type {
                            default: ty_default,
                            ..
                        },
                        rustdoc_types::GenericParamDefKind::Type {
                            default: underlying_default,
                            ..
                        },
                    ) => {
                        // If the typedef doesn't have the same default values for its generics,
                        // then it isn't equivalent to the underlying and so isn't a re-export.
                        if ty_default != underlying_default {
                            // The defaults have changed.
                            return None;
                        }
                        // We don't care about the other fields.
                        // Generic bounds on typedefs are ignored by rustc and generate a lint.
                    }
                    (
                        rustdoc_types::GenericParamDefKind::Const {
                            type_: ty_type,
                            default: ty_default,
                        },
                        rustdoc_types::GenericParamDefKind::Const {
                            type_: underlying_type,
                            default: underlying_default,
                        },
                    ) => {
                        // If the typedef doesn't have the same default values for its generics,
                        // then it isn't equivalent to the underlying and so isn't a re-export.
                        //
                        // Similarly, if it is in any way possible to change the const generic type,
                        // that makes the typedef not a re-export anymore.
                        if ty_default != underlying_default || ty_type != underlying_type {
                            // The generic type or its default has changed.
                            return None;
                        }
                    }
                    _ => {
                        // Not the same kind of generic parameter.
                        return None;
                    }
                }
            }
        }

        Some(underlying)
    } else {
        None
    }
}

#[derive(Debug)]
struct ManualTraitItem {
    name: &'static str,
    is_auto: bool,
    is_unsafe: bool,
}

/// Limiting the creation of manually inlined traits to only those that are used by the lints.
/// There are other foreign traits, but it is not obvious how the manually inlined traits
/// should look like for them.
const MANUAL_TRAIT_ITEMS: [ManualTraitItem; 14] = [
    ManualTraitItem {
        name: "Debug",
        is_auto: false,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "Clone",
        is_auto: false,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "Copy",
        is_auto: false,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "PartialOrd",
        is_auto: false,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "Ord",
        is_auto: false,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "PartialEq",
        is_auto: false,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "Eq",
        is_auto: false,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "Hash",
        is_auto: false,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "Send",
        is_auto: true,
        is_unsafe: true,
    },
    ManualTraitItem {
        name: "Sync",
        is_auto: true,
        is_unsafe: true,
    },
    ManualTraitItem {
        name: "Unpin",
        is_auto: true,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "RefUnwindSafe",
        is_auto: true,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "UnwindSafe",
        is_auto: true,
        is_unsafe: false,
    },
    ManualTraitItem {
        name: "Sized",
        is_auto: false,
        is_unsafe: false,
    },
];

fn new_trait(manual_trait_item: &ManualTraitItem, id: Id, crate_id: u32) -> Item {
    Item {
        id,
        crate_id,
        name: Some(manual_trait_item.name.to_string()),
        span: None,
        visibility: rustdoc_types::Visibility::Public,
        docs: None,
        links: HashMap::new(),
        attrs: Vec::new(),
        deprecation: None,
        inner: rustdoc_types::ItemEnum::Trait(rustdoc_types::Trait {
            is_auto: manual_trait_item.is_auto,
            is_unsafe: manual_trait_item.is_unsafe,
            // The `item`, `generics`, `bounds` and `implementations`
            // are not currently present in the schema,
            // so it is safe to fill them with empty containers,
            // even though some traits in reality have some values in them.
            items: Vec::new(),
            generics: rustdoc_types::Generics {
                params: Vec::new(),
                where_predicates: Vec::new(),
            },
            bounds: Vec::new(),
            implementations: Vec::new(),
        }),
    }
}

fn create_manually_inlined_builtin_traits(crate_: &Crate) -> HashMap<Id, Item> {
    let paths = crate_
        .index
        .values()
        .map(|item| &item.inner)
        .filter_map(|item_enum| match item_enum {
            rustdoc_types::ItemEnum::Impl(impl_) => Some(impl_),
            _ => None,
        })
        .filter_map(|impl_| impl_.trait_.as_ref());

    paths
        .filter_map(|path| {
            MANUAL_TRAIT_ITEMS
                .iter()
                .find(|manual| manual.name == path.name)
                .and_then(|manual| {
                    crate_.paths.get(&path.id).map(|item_summary| {
                        (
                            path.id.clone(),
                            new_trait(manual, path.id.clone(), item_summary.crate_id),
                        )
                    })
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;
    use rustdoc_types::{Crate, Id};

    use crate::{test_util::load_pregenerated_rustdoc, IndexedCrate};

    fn find_item_id<'a>(crate_: &'a Crate, name: &str) -> &'a Id {
        crate_
            .index
            .iter()
            .filter_map(|(id, item)| (item.name.as_deref() == Some(name)).then_some(id))
            .exactly_one()
            .expect("exactly one matching name")
    }

    /// Ensure that methods, consts, and fields within structs are not importable.
    #[test]
    fn structs_are_not_modules() {
        let rustdoc = load_pregenerated_rustdoc("structs_are_not_modules");
        let indexed_crate = IndexedCrate::new(&rustdoc);

        let top_level_function = find_item_id(&rustdoc, "top_level_function");
        let method = find_item_id(&rustdoc, "method");
        let associated_fn = find_item_id(&rustdoc, "associated_fn");
        let field = find_item_id(&rustdoc, "field");
        let const_item = find_item_id(&rustdoc, "THE_ANSWER");

        // All the items are public.
        assert!(indexed_crate
            .visibility_forest
            .contains_key(top_level_function));
        assert!(indexed_crate.visibility_forest.contains_key(method));
        assert!(indexed_crate.visibility_forest.contains_key(associated_fn));
        assert!(indexed_crate.visibility_forest.contains_key(field));
        assert!(indexed_crate.visibility_forest.contains_key(const_item));

        // But only `top_level_function` is importable.
        assert_eq!(
            vec![vec!["structs_are_not_modules", "top_level_function"]],
            indexed_crate.publicly_importable_names(top_level_function)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(method)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(associated_fn)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(field)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(const_item)
        );
    }

    /// Ensure that methods and consts within enums are not importable.
    /// However, enum variants are the exception: they are importable!
    #[test]
    fn enums_are_not_modules() {
        let rustdoc = load_pregenerated_rustdoc("enums_are_not_modules");
        let indexed_crate = IndexedCrate::new(&rustdoc);

        let top_level_function = find_item_id(&rustdoc, "top_level_function");
        let variant = find_item_id(&rustdoc, "Variant");
        let method = find_item_id(&rustdoc, "method");
        let associated_fn = find_item_id(&rustdoc, "associated_fn");
        let const_item = find_item_id(&rustdoc, "THE_ANSWER");

        // All the items are public.
        assert!(indexed_crate
            .visibility_forest
            .contains_key(top_level_function));
        assert!(indexed_crate.visibility_forest.contains_key(variant));
        assert!(indexed_crate.visibility_forest.contains_key(method));
        assert!(indexed_crate.visibility_forest.contains_key(associated_fn));
        assert!(indexed_crate.visibility_forest.contains_key(const_item));

        // But only `top_level_function` and `Foo::variant` is importable.
        assert_eq!(
            vec![vec!["enums_are_not_modules", "top_level_function"]],
            indexed_crate.publicly_importable_names(top_level_function)
        );
        assert_eq!(
            vec![vec!["enums_are_not_modules", "Foo", "Variant"]],
            indexed_crate.publicly_importable_names(variant)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(method)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(associated_fn)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(const_item)
        );
    }

    /// Ensure that methods, consts, and fields within unions are not importable.
    #[test]
    fn unions_are_not_modules() {
        let rustdoc = load_pregenerated_rustdoc("unions_are_not_modules");
        let indexed_crate = IndexedCrate::new(&rustdoc);

        let top_level_function = find_item_id(&rustdoc, "top_level_function");
        let method = find_item_id(&rustdoc, "method");
        let associated_fn = find_item_id(&rustdoc, "associated_fn");
        let left_field = find_item_id(&rustdoc, "left");
        let right_field = find_item_id(&rustdoc, "right");
        let const_item = find_item_id(&rustdoc, "THE_ANSWER");

        // All the items are public.
        assert!(indexed_crate
            .visibility_forest
            .contains_key(top_level_function));
        assert!(indexed_crate.visibility_forest.contains_key(method));
        assert!(indexed_crate.visibility_forest.contains_key(associated_fn));
        assert!(indexed_crate.visibility_forest.contains_key(left_field));
        assert!(indexed_crate.visibility_forest.contains_key(right_field));
        assert!(indexed_crate.visibility_forest.contains_key(const_item));

        // But only `top_level_function` is importable.
        assert_eq!(
            vec![vec!["unions_are_not_modules", "top_level_function"]],
            indexed_crate.publicly_importable_names(top_level_function)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(method)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(associated_fn)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(left_field)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(right_field)
        );
        assert_eq!(
            Vec::<Vec<&str>>::new(),
            indexed_crate.publicly_importable_names(const_item)
        );
    }

    mod reexports {
        use std::collections::{BTreeMap, BTreeSet};

        use itertools::Itertools;
        use maplit::{btreemap, btreeset};

        use crate::{test_util::load_pregenerated_rustdoc, IndexedCrate};

        fn assert_exported_items_match(
            test_crate: &str,
            expected_items: &BTreeMap<&str, BTreeSet<&str>>,
        ) {
            let rustdoc = load_pregenerated_rustdoc(test_crate);
            let indexed_crate = IndexedCrate::new(&rustdoc);

            for (&expected_item_name, expected_importable_paths) in expected_items {
                assert!(
                    !expected_item_name.contains(':'),
                    "only direct item names can be checked at the moment: {expected_item_name}"
                );

                let item_id_candidates = rustdoc
                    .index
                    .iter()
                    .filter_map(|(id, item)| {
                        (item.name.as_deref() == Some(expected_item_name)).then_some(id)
                    })
                    .collect_vec();
                if item_id_candidates.len() != 1 {
                    panic!(
                        "Expected to find exactly one item with name {expected_item_name}, \
                        but found these matching IDs: {item_id_candidates:?}"
                    );
                }
                let item_id = item_id_candidates[0];
                let actual_items: Vec<_> = indexed_crate
                    .publicly_importable_names(item_id)
                    .into_iter()
                    .map(|components| components.into_iter().join("::"))
                    .collect();
                let deduplicated_actual_items: BTreeSet<_> =
                    actual_items.iter().map(|x| x.as_str()).collect();
                assert_eq!(
                    actual_items.len(),
                    deduplicated_actual_items.len(),
                    "duplicates found: {actual_items:?}"
                );

                assert_eq!(expected_importable_paths, &deduplicated_actual_items);
            }
        }

        /// Allows testing for items with overlapping names, such as a function and a type
        /// with the same name (which Rust considers in separate namespaces).
        fn assert_duplicated_exported_items_match(
            test_crate: &str,
            expected_items_and_counts: &BTreeMap<&str, (usize, BTreeSet<&str>)>,
        ) {
            let rustdoc = load_pregenerated_rustdoc(test_crate);
            let indexed_crate = IndexedCrate::new(&rustdoc);

            for (&expected_item_name, (expected_count, expected_importable_paths)) in
                expected_items_and_counts
            {
                assert!(
                    !expected_item_name.contains(':'),
                    "only direct item names can be checked at the moment: {expected_item_name}"
                );

                let item_id_candidates = rustdoc
                    .index
                    .iter()
                    .filter_map(|(id, item)| {
                        (item.name.as_deref() == Some(expected_item_name)).then_some(id)
                    })
                    .collect_vec();
                if item_id_candidates.len() != *expected_count {
                    panic!(
                        "Expected to find exactly {expected_count} items with name \
                        {expected_item_name}, but found these matching IDs: {item_id_candidates:?}"
                    );
                }
                for item_id in item_id_candidates {
                    let actual_items: Vec<_> = indexed_crate
                        .publicly_importable_names(item_id)
                        .into_iter()
                        .map(|components| components.into_iter().join("::"))
                        .collect();
                    let deduplicated_actual_items: BTreeSet<_> =
                        actual_items.iter().map(|x| x.as_str()).collect();
                    assert_eq!(
                        actual_items.len(),
                        deduplicated_actual_items.len(),
                        "duplicates found: {actual_items:?}"
                    );
                    assert_eq!(expected_importable_paths, &deduplicated_actual_items);
                }
            }
        }

        #[test]
        fn pub_inside_pub_crate_mod() {
            let test_crate = "pub_inside_pub_crate_mod";
            let expected_items = btreemap! {
                "Foo" => btreeset![],
                "Bar" => btreeset![
                    "pub_inside_pub_crate_mod::Bar",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn reexport() {
            let test_crate = "reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "reexport::foo",
                    "reexport::inner::foo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn reexport_from_private_module() {
            let test_crate = "reexport_from_private_module";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "reexport_from_private_module::foo",
                ],
                "Bar" => btreeset![
                    "reexport_from_private_module::Bar",
                ],
                "Baz" => btreeset![
                    "reexport_from_private_module::nested::Baz",
                ],
                "quux" => btreeset![
                    "reexport_from_private_module::quux",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn renaming_reexport() {
            let test_crate = "renaming_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "renaming_reexport::bar",
                    "renaming_reexport::inner::foo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn renaming_reexport_of_reexport() {
            let test_crate = "renaming_reexport_of_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "renaming_reexport_of_reexport::bar",
                    "renaming_reexport_of_reexport::foo",
                    "renaming_reexport_of_reexport::inner::foo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn renaming_mod_reexport() {
            let test_crate = "renaming_mod_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "renaming_mod_reexport::inner::a::foo",
                    "renaming_mod_reexport::inner::b::foo",
                    "renaming_mod_reexport::direct::foo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn glob_reexport() {
            let test_crate = "glob_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "glob_reexport::foo",
                    "glob_reexport::inner::foo",
                ],
                "Bar" => btreeset![
                    "glob_reexport::Bar",
                    "glob_reexport::inner::Bar",
                ],
                "nested" => btreeset![
                    "glob_reexport::nested",
                ],
                "Baz" => btreeset![
                    "glob_reexport::Baz",
                ],
                "First" => btreeset![
                    "glob_reexport::First",
                    "glob_reexport::Baz::First",
                ],
                "Second" => btreeset![
                    "glob_reexport::Second",
                    "glob_reexport::Baz::Second",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn glob_of_glob_reexport() {
            let test_crate = "glob_of_glob_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "glob_of_glob_reexport::foo",
                ],
                "Bar" => btreeset![
                    "glob_of_glob_reexport::Bar",
                ],
                "Baz" => btreeset![
                    "glob_of_glob_reexport::Baz",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn glob_of_renamed_reexport() {
            let test_crate = "glob_of_renamed_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "glob_of_renamed_reexport::renamed_foo",
                ],
                "Bar" => btreeset![
                    "glob_of_renamed_reexport::RenamedBar",
                ],
                "First" => btreeset![
                    "glob_of_renamed_reexport::RenamedFirst",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn glob_reexport_enum_variants() {
            let test_crate = "glob_reexport_enum_variants";
            let expected_items = btreemap! {
                "First" => btreeset![
                    "glob_reexport_enum_variants::First",
                ],
                "Second" => btreeset![
                    "glob_reexport_enum_variants::Second",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn glob_reexport_cycle() {
            let test_crate = "glob_reexport_cycle";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    "glob_reexport_cycle::first::foo",
                    "glob_reexport_cycle::second::foo",
                ],
                "Bar" => btreeset![
                    "glob_reexport_cycle::first::Bar",
                    "glob_reexport_cycle::second::Bar",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn infinite_recursive_reexport() {
            let test_crate = "infinite_recursive_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    // We don't want to expand all infinitely-many names here.
                    // We only return cycle-free paths, which are the following:
                    "infinite_recursive_reexport::foo",
                    "infinite_recursive_reexport::inner::foo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn infinite_indirect_recursive_reexport() {
            let test_crate = "infinite_indirect_recursive_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    // We don't want to expand all infinitely-many names here.
                    // We only return cycle-free paths, which are the following:
                    "infinite_indirect_recursive_reexport::foo",
                    "infinite_indirect_recursive_reexport::nested::foo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn infinite_corecursive_reexport() {
            let test_crate = "infinite_corecursive_reexport";
            let expected_items = btreemap! {
                "foo" => btreeset![
                    // We don't want to expand all infinitely-many names here.
                    // We only return cycle-free paths, which are the following:
                    "infinite_corecursive_reexport::a::foo",
                    "infinite_corecursive_reexport::b::a::foo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_type_alias_reexport() {
            let test_crate = "pub_type_alias_reexport";
            let expected_items = btreemap! {
                "Foo" => btreeset![
                    "pub_type_alias_reexport::Exported",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_generic_type_alias_reexport() {
            let test_crate = "pub_generic_type_alias_reexport";
            let expected_items = btreemap! {
                "Foo" => btreeset![
                    // Only `Exported` and `ExportedRenamedParams` are re-exports.
                    //
                    //`ExportedRenamedParams` renames the generic parameters
                    // but does not change their meaning.
                    //
                    // `ExportedWithDefaults` is not a re-export because it adds
                    //
                    // The other type aliases are not equivalent since they constrain
                    // some of the underlying type's generic parameters.
                    "pub_generic_type_alias_reexport::Exported",
                    "pub_generic_type_alias_reexport::ExportedRenamedParams",
                ],
                "Exported" => btreeset![
                    // The type alias itself is also a visible item.
                    "pub_generic_type_alias_reexport::Exported",
                ],
                "ExportedWithDefaults" => btreeset![
                    // The type alias itself is also a visible item.
                    "pub_generic_type_alias_reexport::ExportedWithDefaults",
                ],
                "ExportedRenamedParams" => btreeset![
                    // The type alias itself is also a visible item.
                    "pub_generic_type_alias_reexport::ExportedRenamedParams",
                ],
                "ExportedSpecificLifetime" => btreeset![
                    "pub_generic_type_alias_reexport::ExportedSpecificLifetime",
                ],
                "ExportedSpecificType" => btreeset![
                    "pub_generic_type_alias_reexport::ExportedSpecificType",
                ],
                "ExportedSpecificConst" => btreeset![
                    "pub_generic_type_alias_reexport::ExportedSpecificConst",
                ],
                "ExportedFullySpecified" => btreeset![
                    "pub_generic_type_alias_reexport::ExportedFullySpecified",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_generic_type_alias_shuffled_order() {
            let test_crate = "pub_generic_type_alias_shuffled_order";
            let expected_items = btreemap! {
                // The type aliases reverse the generic parameters' orders,
                // so they are not re-exports of the underlying types.
                "GenericFoo" => btreeset![
                    "pub_generic_type_alias_shuffled_order::inner::GenericFoo",
                ],
                "LifetimeFoo" => btreeset![
                    "pub_generic_type_alias_shuffled_order::inner::LifetimeFoo",
                ],
                "ConstFoo" => btreeset![
                    "pub_generic_type_alias_shuffled_order::inner::ConstFoo",
                ],
                "ReversedGenericFoo" => btreeset![
                    "pub_generic_type_alias_shuffled_order::ReversedGenericFoo",
                ],
                "ReversedLifetimeFoo" => btreeset![
                    "pub_generic_type_alias_shuffled_order::ReversedLifetimeFoo",
                ],
                "ReversedConstFoo" => btreeset![
                    "pub_generic_type_alias_shuffled_order::ReversedConstFoo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_generic_type_alias_added_defaults() {
            let test_crate = "pub_generic_type_alias_added_defaults";
            let expected_items = btreemap! {
                "Foo" => btreeset![
                    "pub_generic_type_alias_added_defaults::inner::Foo",
                ],
                "Bar" => btreeset![
                    "pub_generic_type_alias_added_defaults::inner::Bar",
                ],
                "DefaultFoo" => btreeset![
                    "pub_generic_type_alias_added_defaults::DefaultFoo",
                ],
                "DefaultBar" => btreeset![
                    "pub_generic_type_alias_added_defaults::DefaultBar",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_generic_type_alias_changed_defaults() {
            let test_crate = "pub_generic_type_alias_changed_defaults";
            let expected_items = btreemap! {
                // The type aliases change the default values of the generic parameters,
                // so they are not re-exports of the underlying types.
                "Foo" => btreeset![
                    "pub_generic_type_alias_changed_defaults::inner::Foo",
                ],
                "Bar" => btreeset![
                    "pub_generic_type_alias_changed_defaults::inner::Bar",
                ],
                "ExportedWithoutTypeDefault" => btreeset![
                    "pub_generic_type_alias_changed_defaults::ExportedWithoutTypeDefault",
                ],
                "ExportedWithoutConstDefault" => btreeset![
                    "pub_generic_type_alias_changed_defaults::ExportedWithoutConstDefault",
                ],
                "ExportedWithoutDefaults" => btreeset![
                    "pub_generic_type_alias_changed_defaults::ExportedWithoutDefaults",
                ],
                "ExportedWithDifferentTypeDefault" => btreeset![
                    "pub_generic_type_alias_changed_defaults::ExportedWithDifferentTypeDefault",
                ],
                "ExportedWithDifferentConstDefault" => btreeset![
                    "pub_generic_type_alias_changed_defaults::ExportedWithDifferentConstDefault",
                ],
                "ExportedWithDifferentDefaults" => btreeset![
                    "pub_generic_type_alias_changed_defaults::ExportedWithDifferentDefaults",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_generic_type_alias_same_signature_but_not_equivalent() {
            let test_crate = "pub_generic_type_alias_same_signature_but_not_equivalent";
            let expected_items = btreemap! {
                "GenericFoo" => btreeset![
                    "pub_generic_type_alias_same_signature_but_not_equivalent::inner::GenericFoo",
                ],
                "ChangedFoo" => btreeset![
                    "pub_generic_type_alias_same_signature_but_not_equivalent::ChangedFoo",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_type_alias_of_type_alias() {
            let test_crate = "pub_type_alias_of_type_alias";
            let expected_items = btreemap! {
                "Foo" => btreeset![
                    "pub_type_alias_of_type_alias::inner::Foo",
                    "pub_type_alias_of_type_alias::inner::AliasedFoo",
                    "pub_type_alias_of_type_alias::ExportedFoo",
                ],
                "Bar" => btreeset![
                    "pub_type_alias_of_type_alias::inner::Bar",
                    "pub_type_alias_of_type_alias::inner::AliasedBar",
                    "pub_type_alias_of_type_alias::ExportedBar",
                ],
                "AliasedFoo" => btreeset![
                    "pub_type_alias_of_type_alias::inner::AliasedFoo",
                    "pub_type_alias_of_type_alias::ExportedFoo",
                ],
                "AliasedBar" => btreeset![
                    "pub_type_alias_of_type_alias::inner::AliasedBar",
                    "pub_type_alias_of_type_alias::ExportedBar",
                ],
                "ExportedFoo" => btreeset![
                    "pub_type_alias_of_type_alias::ExportedFoo",
                ],
                "ExportedBar" => btreeset![
                    "pub_type_alias_of_type_alias::ExportedBar",
                ],
                "DifferentLifetimeBar" => btreeset![
                    "pub_type_alias_of_type_alias::DifferentLifetimeBar",
                ],
                "DifferentGenericBar" => btreeset![
                    "pub_type_alias_of_type_alias::DifferentGenericBar",
                ],
                "DifferentConstBar" => btreeset![
                    "pub_type_alias_of_type_alias::DifferentConstBar",
                ],
                "ReorderedBar" => btreeset![
                    "pub_type_alias_of_type_alias::ReorderedBar",
                ],
                "DefaultValueBar" => btreeset![
                    "pub_type_alias_of_type_alias::DefaultValueBar",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_type_alias_of_composite_type() {
            let test_crate = "pub_type_alias_of_composite_type";
            let expected_items = btreemap! {
                "Foo" => btreeset![
                    "pub_type_alias_of_composite_type::inner::Foo",
                ],
                "I64Tuple" => btreeset![
                    "pub_type_alias_of_composite_type::I64Tuple",
                ],
                "MixedTuple" => btreeset![
                    "pub_type_alias_of_composite_type::MixedTuple",
                ],
                "GenericTuple" => btreeset![
                    "pub_type_alias_of_composite_type::GenericTuple",
                ],
                "LifetimeTuple" => btreeset![
                    "pub_type_alias_of_composite_type::LifetimeTuple",
                ],
                "ConstTuple" => btreeset![
                    "pub_type_alias_of_composite_type::ConstTuple",
                ],
                "DefaultGenericTuple" => btreeset![
                    "pub_type_alias_of_composite_type::DefaultGenericTuple",
                ],
                "DefaultConstTuple" => btreeset![
                    "pub_type_alias_of_composite_type::DefaultConstTuple",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn pub_generic_type_alias_omitted_default() {
            let test_crate = "pub_generic_type_alias_omitted_default";
            let expected_items = btreemap! {
                "DefaultConst" => btreeset![
                    "pub_generic_type_alias_omitted_default::inner::DefaultConst",
                ],
                "DefaultType" => btreeset![
                    "pub_generic_type_alias_omitted_default::inner::DefaultType",
                ],
                "ConstOnly" => btreeset![
                    "pub_generic_type_alias_omitted_default::inner::ConstOnly",
                ],
                "TypeOnly" => btreeset![
                    "pub_generic_type_alias_omitted_default::inner::TypeOnly",
                ],
                "OmittedConst" => btreeset![
                    "pub_generic_type_alias_omitted_default::OmittedConst",
                ],
                "OmittedType" => btreeset![
                    "pub_generic_type_alias_omitted_default::OmittedType",
                ],
                "NonGenericConst" => btreeset![
                    "pub_generic_type_alias_omitted_default::NonGenericConst",
                ],
                "NonGenericType" => btreeset![
                    "pub_generic_type_alias_omitted_default::NonGenericType",
                ],
            };

            assert_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn type_and_value_with_matching_names() {
            let test_crate = "type_and_value_with_matching_names";
            let expected_items = btreemap! {
                "Foo" => (2, btreeset![
                    "type_and_value_with_matching_names::Foo",
                    "type_and_value_with_matching_names::nested::Foo",
                ]),
                "Bar" => (2, btreeset![
                    "type_and_value_with_matching_names::Bar",
                    "type_and_value_with_matching_names::nested::Bar",
                ]),
            };

            assert_duplicated_exported_items_match(test_crate, &expected_items);
        }

        #[test]
        fn explicit_reexport_of_matching_names() {
            if version_check::is_min_version("1.69.0").unwrap_or(true) {
                let test_crate = "explicit_reexport_of_matching_names";
                let expected_items = btreemap! {
                    "Foo" => (2, btreeset![
                        "explicit_reexport_of_matching_names::Bar",
                        "explicit_reexport_of_matching_names::Foo",
                        "explicit_reexport_of_matching_names::nested::Foo",
                    ]),
                };

                assert_duplicated_exported_items_match(test_crate, &expected_items);
            } else {
                use std::io::Write;
                writeln!(
                    std::io::stderr(),
                    "skipping 'explicit_reexport_of_matching_names' test due to Rust {:?}",
                    version_check::Version::read(),
                )
                .expect("write failed");
            }
        }
    }
}
