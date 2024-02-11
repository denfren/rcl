// RCL -- A reasonable configuration language.
// Copyright 2023 Ruud van Asseldonk

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// A copy of the License has been included in the root of the repository.

//! Type requirements, a central building block of typechecking.

use std::rc::Rc;

use crate::error::{Error, IntoError, PathElement, Result};
use crate::fmt_rcl::format_rcl;
use crate::fmt_type::format_type;
use crate::pprint::{concat, indent, Doc};
use crate::runtime::Value;
use crate::source::Span;
use crate::types::{report_type_mismatch, Dict, Function, Type};

/// A type requirement.
///
/// A [`Type`] is a type that the typechecker inferred. A [`TypeReq`] is a
/// requirement that the typechecker needs to fulfill. Type requirements
/// correspond to types, but carry additional context about _why_ that type was
/// expected in a particular location. (Context such as “because it was part of
/// a type annotation at this span”, or “because conditions should be booleans”.)
/// Requirements can be fulfilled by subtypes of the required type.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TypeReq {
    /// We have no requirement on the type, any value is allowed.
    None,

    /// The type was required due to a type annotation.
    Annotation(Span, ReqType),

    /// A boolean was required because it's used as a condition.
    Condition,

    /// The type was required due to an operator.
    Operator(Span, ReqType),

    /// An integer is required due to indexing into a list.
    IndexList,
}

/// The types that can occur in type requirements.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ReqType {
    /// Require [`Type::Bool`].
    Bool,

    /// Require [`Type::Int`].
    Int,

    /// Require [`Type::Null`].
    Null,

    /// Require [`Type::String`].
    String,

    /// Require [`Type::List`].
    List(Rc<TypeReq>),

    /// Require [`Type::Set`].
    Set(Rc<TypeReq>),

    /// Require [`Type::Dict`].
    Dict(Rc<DictReq>),

    /// Require [`Type::Function`].
    Function(Rc<FunctionReq>),
}

/// The type parameter requirements for the `Dict` type.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DictReq {
    pub key: TypeReq,
    pub value: TypeReq,
}

/// A function type requirement.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FunctionReq {
    pub args: Vec<TypeReq>,
    pub result: TypeReq,
}

/// The result of a static typecheck.
///
/// A diff can represent type errors, nested type errors, no error, or a signal
/// that the check could not be performed statically and needs to be deferred to
/// runtime.
#[derive(Debug)]
pub enum TypeDiff {
    /// No error. The actual type matches the expected type.
    Ok(Type),

    /// The check could not be performed statically, a runtime check is needed.
    Defer(Type),

    /// A static type mismatch that cannot be broken down further.
    ///
    /// The requirement specifies the expected type and the reason for expecting
    /// it, the other type is the actual type that we encountered.
    Error(TypeReq, Type),

    /// There is a type mismatch in the element type of a list.
    List(Box<TypeDiff>),

    /// There is a type mismatch in the element type of a set.
    Set(Box<TypeDiff>),

    /// There is a type mismatch somewhere in the dict type.
    Dict(Box<TypeDiff>, Box<TypeDiff>),

    /// There is a type mismatch somewhere in a function type.
    Function(Vec<TypeDiff>, Box<TypeDiff>),
}

impl ReqType {
    /// Return the `Type` of a value when this requirement is satisfied.
    ///
    /// This function is potentially expensive because it does a deep traversal
    /// of the type requirement.
    fn to_type(&self) -> Type {
        match self {
            ReqType::Null => Type::Null,
            ReqType::Bool => Type::Bool,
            ReqType::Int => Type::Int,
            ReqType::String => Type::String,
            ReqType::List(t) => Type::List(t.to_type().into()),
            ReqType::Set(t) => Type::Set(t.to_type().into()),
            ReqType::Dict(kv) => {
                let dict = Dict {
                    key: kv.key.to_type(),
                    value: kv.value.to_type(),
                };
                Type::Dict(dict.into())
            }
            ReqType::Function(f) => {
                let fn_type = Function {
                    args: f.args.iter().map(|arg_req| arg_req.to_type()).collect(),
                    result: f.result.to_type(),
                };
                Type::Function(fn_type.into())
            }
        }
    }

    // TODO: Having to feed in the `TypeReq`, while the `ReqType` is derived
    // from it, feels wrong. There *has* to be an elegant way to express this,
    // but I haven't discovered it yet.
    fn check_type(&self, req: &TypeReq, type_: &Type) -> TypeDiff {
        match (self, type_) {
            // If there was some requirement, but we don't know the type,
            // then we have to defer the typecheck to runtime.
            (_, Type::Dynamic) => TypeDiff::Defer(self.to_type()),

            // For the primitive types, we just check for matching values.
            (ReqType::Null, Type::Null) => TypeDiff::Ok(type_.clone()),
            (ReqType::Bool, Type::Bool) => TypeDiff::Ok(type_.clone()),
            (ReqType::Int, Type::Int) => TypeDiff::Ok(type_.clone()),
            (ReqType::String, Type::String) => TypeDiff::Ok(type_.clone()),

            // For compound types, we need to do the inner inspection.
            (ReqType::List(elem_req), Type::List(elem_type)) => {
                match elem_req.check_type_impl(elem_type) {
                    TypeDiff::Ok(..) => TypeDiff::Ok(type_.clone()),
                    TypeDiff::Defer(t) => TypeDiff::Defer(Type::List(t.into())),
                    error => TypeDiff::List(error.into()),
                }
            }
            (ReqType::Set(elem_req), Type::Set(elem_type)) => {
                match elem_req.check_type_impl(elem_type) {
                    TypeDiff::Ok(..) => TypeDiff::Ok(type_.clone()),
                    TypeDiff::Defer(t) => TypeDiff::Defer(Type::Set(t.into())),
                    error => TypeDiff::Set(error.into()),
                }
            }
            (ReqType::Dict(kv_req), Type::Dict(kv_type)) => {
                let k_diff = kv_req.key.check_type_impl(&kv_type.key);
                let v_diff = kv_req.value.check_type_impl(&kv_type.value);
                match (k_diff, v_diff) {
                    (TypeDiff::Ok(..), TypeDiff::Ok(..)) => TypeDiff::Ok(type_.clone()),
                    (
                        TypeDiff::Ok(tk) | TypeDiff::Defer(tk),
                        TypeDiff::Ok(tv) | TypeDiff::Defer(tv),
                    ) => {
                        let dict = Dict { key: tk, value: tv };
                        TypeDiff::Defer(Type::Dict(dict.into()))
                    }
                    (k_diff, v_diff) => TypeDiff::Dict(k_diff.into(), v_diff.into()),
                }
            }
            (ReqType::Function(fn_req), Type::Function(fn_type)) => {
                if fn_req.args.len() != fn_type.args.len() {
                    return TypeDiff::Error(req.clone(), type_.clone());
                }

                let mut arg_diffs = Vec::with_capacity(fn_req.args.len());

                for (arg_req, arg_type) in fn_req.args.iter().zip(&fn_type.args) {
                    // TODO: To be properly generic here, we have to allow the
                    // arguments to be contravariant. Instead of the arg type
                    // satisfying the requirement (being a subtype of it), it
                    // has to be the other way around: the requirements have to
                    // be subtypes of the actual arguments. But we don't have a
                    // way do that right now, so I'm going to go for just equality,
                    // which may reject some correct programs but is at least safe.
                    if &arg_req.to_type() != arg_type {
                        arg_diffs.push(TypeDiff::Error(arg_req.clone(), arg_type.clone()));
                    } else {
                        arg_diffs.push(TypeDiff::Ok(arg_type.clone()));
                    }
                }

                match fn_req.result.check_type_impl(&fn_type.result) {
                    TypeDiff::Ok(..) => TypeDiff::Ok(type_.clone()),
                    TypeDiff::Defer(t) => {
                        let fn_type = Function {
                            args: fn_type.args.clone(),
                            result: t,
                        };
                        TypeDiff::Defer(Type::Function(fn_type.into()))
                    }
                    error => TypeDiff::Function(arg_diffs, error.into()),
                }
            }

            // If we did not match anything, then this is a type error.
            _ => TypeDiff::Error(req.clone(), type_.clone()),
        }
    }
}

/// The result of a static typecheck.
pub enum Typed {
    /// The type is known statically, and this is the most specific type we infer.
    Type(Type),

    /// We can't check this statically, a runtime check is needed.
    ///
    /// If the runtime check passes, then the value fits the returned type.
    Defer(Type),
}

impl TypeReq {
    /// Return the type required by this requirement.
    pub fn req_type(&self) -> Option<&ReqType> {
        match self {
            TypeReq::None => None,
            TypeReq::Annotation(.., t) => Some(t),
            TypeReq::Condition => Some(&ReqType::Bool),
            TypeReq::Operator(.., t) => Some(t),
            TypeReq::IndexList => Some(&ReqType::Int),
        }
    }

    /// Return the most precise type that describes any value that satisfies this requirement.
    pub fn to_type(&self) -> Type {
        match self.req_type() {
            None => Type::Dynamic,
            Some(t) => t.to_type(),
        }
    }

    /// Statically check that the given type is a subtype of the required type.
    fn check_type_impl(&self, type_: &Type) -> TypeDiff {
        match self.req_type() {
            None => TypeDiff::Ok(type_.clone()),
            Some(t) => t.check_type(self, type_),
        }
    }

    /// Explain why the type error is caused.
    pub fn add_context(&self, error: Error) -> Error {
        match self {
            TypeReq::None => unreachable!("If no type was expected, it wouldn't cause an error."),
            TypeReq::Annotation(at, _) => {
                error.with_note(*at, "The expected type is specified here.")
            }
            TypeReq::Condition => {
                error.with_help("There is no implicit conversion, conditions must be boolean.")
            }
            TypeReq::Operator(at, t) if t.to_type().is_atom() => error.with_note(
                *at,
                concat! {
                    "Expected " format_type(&t.to_type()).into_owned() " due to this operator."
                },
            ),
            TypeReq::Operator(..) => unreachable!("We don't have operators with non-atomic types."),
            TypeReq::IndexList => error.with_help("List indices must be integers."),
        }
    }

    /// Statically check that the given type is a subtype of the required type.
    pub fn check_type(&self, at: Span, type_: &Type) -> Result<Typed> {
        match self.check_type_impl(type_) {
            TypeDiff::Ok(t) => Ok(Typed::Type(t)),
            TypeDiff::Defer(t) => Ok(Typed::Defer(t)),
            TypeDiff::Error(expected, actual) => {
                // A top-level type error, we can report with a simple message.
                let err = at
                    .error("Type mismatch.")
                    .with_body(report_type_mismatch(&expected.to_type(), &actual));
                self.add_context(err).err()
            }
            diff => {
                // If the error is nested somewhere inside a type, then we
                // resort to a more complex format where we first print the
                // type itself, with the error part replaced with a placeholder,
                // and then we add a secondary error to explain the placeholder.
                at.error("Type mismatch in type.")
                    .with_body(format!("TODO: Pretty-print: {diff:?}"))
                    .err()
            }
        }
    }

    /// Dynamically check that the given value fits the required type.
    pub fn check_value(&self, at: Span, value: &Value) -> Result<()> {
        let req_type = match self.req_type() {
            None => return Ok(()),
            Some(t) => t,
        };
        match (req_type, value) {
            // For the primitive types, we just check for matching values.
            (ReqType::Null, Value::Null) => Ok(()),
            (ReqType::Bool, Value::Bool(..)) => Ok(()),
            (ReqType::Int, Value::Int(..)) => Ok(()),
            (ReqType::String, Value::String(..)) => Ok(()),

            // For compound types, we descend into them to check.
            (ReqType::List(elem_type), Value::List(elems)) => {
                for (i, elem) in elems.iter().enumerate() {
                    elem_type
                        .check_value(at, elem)
                        .map_err(|err| err.with_path_element(PathElement::Index(i)))?;
                }
                Ok(())
            }
            (ReqType::Set(elem_type), Value::Set(elems)) => {
                for (i, elem) in elems.iter().enumerate() {
                    elem_type.check_value(at, elem).map_err(|err|
                        // Even though sets don't strictly have indexes,
                        // they do have an order, so report the index to
                        // clarify that this is a nested error.
                        err.with_path_element(PathElement::Index(i)))?;
                }
                Ok(())
            }
            (ReqType::Dict(dict), Value::Dict(kvs)) => {
                for (k, v) in kvs.iter() {
                    dict.key.check_value(at, k).map_err(|err| {
                        err.with_path_element(PathElement::Key("TODO: Support any key".into()))
                    })?;
                    dict.value.check_value(at, v).map_err(|err| {
                        err.with_path_element(PathElement::Key("TODO: Support any key".into()))
                    })?;
                }
                Ok(())
            }

            // TODO: Typecheck functions.
            _ => at
                .error("Type mismatch.")
                .with_body(concat! {
                    "Expected a value that fits this type:"
                    Doc::HardBreak Doc::HardBreak
                    indent! { format_type(&req_type.to_type()).into_owned() }
                    Doc::HardBreak Doc::HardBreak
                    "But got this value:"
                    Doc::HardBreak Doc::HardBreak
                    indent! { format_rcl(value).into_owned() }
                })
                .err(),
        }
    }
}
