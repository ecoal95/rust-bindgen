use super::comp::CompInfo;
use super::enum_ty::Enum;
use super::function::FunctionSig;
use super::item::{Item, ItemId};
use super::int::IntKind;
use super::layout::Layout;
use super::context::BindgenContext;
use super::context::TypeResolver;
use parse::{ClangItemParser, ClangSubItemParser, ParseError};
use clang;

#[derive(Debug)]
pub struct Type {
    /// The name of the type, or None if it was an unnamed struct or union.
    name: Option<String>,
    /// The layout of the type, if known.
    layout: Option<Layout>,
    /// Whether this type is marked as opaque.
    opaque: bool,
    /// Whether this type is marked as hidden.
    hide: bool,
    /// The inner kind of the type
    kind: TypeKind,
    /// Whether this type was declared in a top level context, that is, a module
    /// or the root, or inside another class, mainly a C++ class or struct.
    ///
    /// If it's not top-level, it won't be generated automatically, and should
    /// be generated by the class where it was declared.
    is_toplevel: bool,
}

const RUST_DERIVE_IN_ARRAY_LIMIT: usize = 32usize;

impl Type {
    pub fn new(name: Option<String>,
               layout: Option<Layout>,
               kind: TypeKind) -> Self {
        Type {
            name: name,
            layout: layout,
            opaque: false,
            hide: false,
            kind: kind,
            is_toplevel: false,
        }
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_ref().map(|name| &**name)
    }

    pub fn layout(&self, type_resolver: &TypeResolver) -> Option<Layout> {
        self.layout.or_else(|| {
            match self.kind {
                TypeKind::Comp(ref ci) => ci.layout(type_resolver),
                _ => None,
            }
        })
    }

    // TODO: This will require more logic once it's done.
    pub fn is_opaque(&self, _type_resolver: &TypeResolver) -> bool {
        self.opaque
    }

    pub fn can_derive_debug(&self, type_resolver: &TypeResolver) -> bool {
        !self.is_opaque(type_resolver) && match self.kind {
            TypeKind::Array(t, len) => {
                len <= RUST_DERIVE_IN_ARRAY_LIMIT &&
                type_resolver.resolve_type(t).can_derive_debug(type_resolver)
            }
            TypeKind::Alias(_, t) => {
                type_resolver.resolve_type(t).can_derive_debug(type_resolver)
            }
            TypeKind::Comp(ref info) => {
                info.can_derive_debug(type_resolver)
            }
            _   => true,
        }
    }

    // For some reason, deriving copies of an array of a type that is not known
    // to be copy is a compile error. e.g.:
    //
    // #[derive(Copy)]
    // struct A<T> {
    //     member: T,
    // }
    //
    // is fine, while:
    //
    // #[derive(Copy)]
    // struct A<T> {
    //     member: [T; 1],
    // }
    //
    // is an error.
    //
    // That's the point of the existence of can_derive_copy_in_array().
    pub fn can_derive_copy_in_array(&self, type_resolver: &TypeResolver) -> bool {
        match self.kind {
            TypeKind::Alias(_, t) |
            TypeKind::Array(t, _) => {
                type_resolver.resolve_type(t)
                             .can_derive_copy_in_array(type_resolver)
            }
            TypeKind::Named(..) => false,
            _ => self.can_derive_copy(type_resolver),
        }
    }

    pub fn can_derive_copy(&self, type_resolver: &TypeResolver) -> bool {
        !self.is_opaque(type_resolver) && match self.kind {
            TypeKind::Array(t, len) => {
                len <= RUST_DERIVE_IN_ARRAY_LIMIT &&
                type_resolver.resolve_type(t).can_derive_copy(type_resolver)
            }
            TypeKind::Alias(_, t) => {
                type_resolver.resolve_type(t).can_derive_copy(type_resolver)
            }
            TypeKind::Comp(ref info) => {
                info.can_derive_copy(type_resolver)
            }
            _ => true,
        }
    }

    pub fn has_destructor(&self, type_resolver: &TypeResolver) -> bool {
        self.is_opaque(type_resolver) || match self.kind {
            TypeKind::Alias(_, t) |
            TypeKind::Array(t, _) => {
                type_resolver.resolve_type(t).has_destructor(type_resolver)
            }
            TypeKind::Comp(ref info) => {
                info.has_destructor(type_resolver)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum FloatKind {
    Float,
    Double,
    LongDouble,
}

#[derive(Debug)]
pub enum TypeKind {
    /// The void type.
    Void,
    /// The nullptr_t type.
    NullPtr,
    /// A compound type, that is, a class, struct, or union.
    Comp(CompInfo),
    /// An integer type, of a given kind. `bool` and `char` are also considered
    /// integers.
    Int(IntKind),
    /// A floating point type.
    Float(FloatKind),
    /// A type alias, with a name, that points to another type.
    Alias(String, ItemId),
    /// An array of a type and a lenght.
    Array(ItemId, usize),
    /// A function type, with a given signature.
    Function(FunctionSig),
    /// An enum type.
    Enum(Enum),
    /// A pointer to a type.
    Pointer(ItemId),
    /// A reference to a type.
    Reference(ItemId),
    /// A named type, that is, a template parameter.
    Named(String),
}

impl ClangSubItemParser for Type {
    fn parse(cursor: clang::Cursor, ctx: &mut BindgenContext) -> Option<Self> {
        use clangll::*;

        let ty = cursor.cur_type();
        if ty.kind() == CXType_Invalid {
            return None;
        }

        if let Some(TypeResult::New(item, _decl)) = Self::from_clang_ty(&ty, ctx) {
            return Some(item);
        }

        return None;
        // match cursor.kind() {
        //     CXCursor_UnionDecl |
        //     CXCursor_ClassTemplate |
        //     CXCursor_ClassDecl |
        //     CXCursor_StructDecl => {
        //         let ty = CompInfo::parse(cursor, ctx).unwrap();
        //     }
        // };
    }
}

pub enum TypeResult {
    AlreadyResolved(ItemId),
    New(Type, clang::Cursor),
}

impl Type {
    pub fn from_clang_ty(ty: &clang::Type, ctx: &mut BindgenContext) -> Result<TypeResult, ParseError> {
        use clangll::*;
        if let Some(ty) = ctx.builtin_or_resolved_ty(ty) {
            return Ok(TypeResult::AlreadyResolved(ty));
        }

        let layout = ty.fallible_layout().ok(); // TODO: Do something (log?) the error!
        // XXX This might not be correct always.
        let cursor = ty.declaration();
        let kind = match ty.kind() {
            CXType_Invalid => {
                // XXX: Old bindgen returned void here, I hope to bring back
                // some sanity, but, heh.
                println!("invalid type `{}`", ty.spelling());
                return None;
            }
            CXType_Pointer => {
                let inner = Item::from_ty(&ty.pointee_type(), ctx)
                                .expect("Not able to resolve pointee?");
                TypeKind::Pointer(inner)
            }
            CXType_LValueReference => {
                let inner = Item::from_ty(&ty.pointee_type(), ctx)
                                .expect("Not able to resolve pointee?");
                TypeKind::Reference(inner)
            }
            // XXX DependentSizedArray is wrong
            CXType_VariableArray |
            CXType_DependentSizedArray |
            CXType_IncompleteArray => {
                let inner = Item::from_ty(&ty.elem_type(), ctx)
                                .expect("Not able to resolve array element?");
                TypeKind::Pointer(inner)
            }
            CXType_FunctionProto => {
                let signature = FunctionSig::from_ty(ty, &cursor, ctx)
                                    .expect("Not able to resolve signature?");
                TypeKind::Function(signature)
            }
            CXType_Record |
            CXType_Typedef  |
            CXType_Unexposed |
            CXType_Enum => {
                // return Item::parse(ty.declaration(), ctx).map(TypeResult::AlreadyResolved)
                return Err(ParseError::Recurse);
            }
            CXType_ConstantArray => {
                let inner = Item::from_ty(&ty.elem_type(), ctx)
                                .expect("Not able to resolve array element?");
                TypeKind::Array(inner, ty.array_size())
            }
            #[cfg(not(feature="llvm_stable"))]
            CXType_Elaborated => {
                return Self::from_clang_ty(&ty.named(), ctx);
            }
            _ => {
                use clang::type_to_str;
                warn!("unsupported type `{}`", type_to_str(ty.kind()));
                return Err(ParseError::Continue);
            }
        };

        // TODO: fill the name if appropriate!
        Ok(TypeResult::New(Type::new(None, layout, kind), cursor))
    }
}
