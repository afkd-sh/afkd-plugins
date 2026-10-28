//! A checker of GraphQL requests against Linear's schema, as `tests/schema.graphql` excerpts
//! it: both halves of a request — the document's selection, its variable types and every
//! argument and inline literal at every depth, and the variables sent with it.
//!
//! std and `serde_json` only, so the plugin's unit tests (`src/main.rs`) and the fake Linear
//! (`fake.rs`, which the drift crate includes too) read the same one. It covers the grammar
//! the documents and the excerpt use, and refuses anything else — a fragment, an alias, a
//! directive, a variable default — so it fails closed. It is deliberately stricter than
//! GraphQL's input coercion: a single value never stands for a one-element list, and an
//! `ID` is a string. Each refusal is one sentence naming where it is.

#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

/// GraphQL's built-in scalars.
const BUILT_IN: &[&str] = &["String", "Int", "Float", "Boolean", "ID"];

/// A type as a field, an argument or a variable declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRef {
    Named(String),
    List(Box<TypeRef>),
    NonNull(Box<TypeRef>),
}

impl TypeRef {
    /// The named type under every list and `!`.
    pub fn base(&self) -> &str {
        match self {
            TypeRef::Named(name) => name,
            TypeRef::List(inner) | TypeRef::NonNull(inner) => inner.base(),
        }
    }
}

impl fmt::Display for TypeRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeRef::Named(name) => write!(f, "{name}"),
            TypeRef::List(inner) => write!(f, "[{inner}]"),
            TypeRef::NonNull(inner) => write!(f, "{inner}!"),
        }
    }
}

/// One variable an operation declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarDecl {
    pub name: String,
    pub ty: TypeRef,
}

/// A checked operation: its name, and the variables it declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub name: String,
    pub variables: Vec<VarDecl>,
}

/// What a type of the excerpt is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Object,
    Input,
    Enum,
    Scalar,
}

/// A field of an object or input type, or an argument: its type, its arguments (an object
/// field's), and whether it declares a default.
#[derive(Debug, Clone)]
struct Field {
    ty: TypeRef,
    args: BTreeMap<String, Field>,
    has_default: bool,
}

#[derive(Debug, Clone)]
struct TypeDef {
    kind: Kind,
    fields: BTreeMap<String, Field>,
    values: BTreeSet<String>,
}

/// The excerpt, read.
#[derive(Debug, Clone)]
pub struct Sdl {
    types: BTreeMap<String, TypeDef>,
}

// --- tokens -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Name(String),
    Punct(char),
    Spread,
    Str(String),
    Int(i64),
    Float(f64),
}

impl fmt::Display for Tok {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tok::Name(n) => write!(f, "`{n}`"),
            Tok::Punct(c) => write!(f, "`{c}`"),
            Tok::Spread => write!(f, "`...`"),
            Tok::Str(s) => write!(f, "{s:?}"),
            Tok::Int(n) => write!(f, "{n}"),
            Tok::Float(n) => write!(f, "{n}"),
        }
    }
}

/// Split GraphQL text into tokens: commas, whitespace and `#` comments are insignificant,
/// and a block string (`"""`) is refused, the excerpt being written without docstrings.
fn tokens(text: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() || c == ',' => {
                chars.next();
            }
            '#' => while chars.next().is_some_and(|c| c != '\n') {},
            '{' | '}' | '(' | ')' | '[' | ']' | ':' | '!' | '=' | '$' | '@' | '|' | '&' => {
                out.push(Tok::Punct(c));
                chars.next();
            }
            '.' => {
                let dots: String = (0..3).filter_map(|_| chars.next()).collect();
                if dots != "..." {
                    return Err(format!("a stray `{dots}`"));
                }
                out.push(Tok::Spread);
            }
            '"' => {
                chars.next();
                let mut s = String::new();
                loop {
                    match chars.next() {
                        None => return Err("an unterminated string".into()),
                        Some('"') if s.is_empty() && chars.peek() == Some(&'"') => {
                            return Err("a block string is refused".into())
                        }
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some(e @ ('"' | '\\' | '/')) => s.push(e),
                            other => return Err(format!("the escape `\\{other:?}` is refused")),
                        },
                        Some(c) => s.push(c),
                    }
                }
                out.push(Tok::Str(s));
            }
            c if c == '-' || c.is_ascii_digit() => {
                let mut n = String::new();
                while let Some(&d) = chars.peek() {
                    if d.is_ascii_digit() || matches!(d, '-' | '+' | '.' | 'e' | 'E') {
                        n.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(match n.parse::<i64>() {
                    Ok(i) => Tok::Int(i),
                    Err(_) => Tok::Float(n.parse().map_err(|_| format!("the number `{n}`"))?),
                });
            }
            c if c == '_' || c.is_ascii_alphabetic() => {
                let mut n = String::new();
                while let Some(&d) = chars.peek() {
                    if d == '_' || d.is_ascii_alphanumeric() {
                        n.push(d);
                        chars.next();
                    } else {
                        break;
                    }
                }
                out.push(Tok::Name(n));
            }
            other => return Err(format!("the character `{other}`")),
        }
    }
    Ok(out)
}

/// A cursor over tokens.
struct Cursor {
    toks: Vec<Tok>,
    at: usize,
}

impl Cursor {
    fn new(text: &str) -> Result<Self, String> {
        Ok(Self {
            toks: tokens(text)?,
            at: 0,
        })
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn next(&mut self) -> Result<Tok, String> {
        let tok = self
            .toks
            .get(self.at)
            .cloned()
            .ok_or_else(|| "the text ends too soon".to_string())?;
        self.at += 1;
        Ok(tok)
    }

    fn at_punct(&self, c: char) -> bool {
        self.peek() == Some(&Tok::Punct(c))
    }

    fn eat(&mut self, c: char) -> bool {
        let here = self.at_punct(c);
        if here {
            self.at += 1;
        }
        here
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        match self.next()? {
            Tok::Punct(p) if p == c => Ok(()),
            other => Err(format!("expected `{c}`, found {other}")),
        }
    }

    fn name(&mut self) -> Result<String, String> {
        match self.next()? {
            Tok::Name(n) => Ok(n),
            other => Err(format!("expected a name, found {other}")),
        }
    }

    /// `Name`, `[T]` or either with `!`.
    fn type_ref(&mut self) -> Result<TypeRef, String> {
        let inner = if self.eat('[') {
            let item = self.type_ref()?;
            self.expect(']')?;
            TypeRef::List(Box::new(item))
        } else {
            TypeRef::Named(self.name()?)
        };
        Ok(if self.eat('!') {
            TypeRef::NonNull(Box::new(inner))
        } else {
            inner
        })
    }

    /// One literal value, variables allowed where `vars` is.
    fn value(&mut self, vars: bool) -> Result<Lit, String> {
        Ok(match self.next()? {
            Tok::Punct('$') if vars => Lit::Var(self.name()?),
            Tok::Punct('$') => return Err("a variable where a constant belongs".into()),
            Tok::Str(s) => Lit::Str(s),
            Tok::Int(n) => Lit::Int(n),
            Tok::Float(n) => Lit::Float(n),
            Tok::Name(n) => match n.as_str() {
                "true" => Lit::Bool(true),
                "false" => Lit::Bool(false),
                "null" => Lit::Null,
                _ => Lit::Enum(n),
            },
            Tok::Punct('[') => {
                let mut items = Vec::new();
                while !self.eat(']') {
                    items.push(self.value(vars)?);
                }
                Lit::List(items)
            }
            Tok::Punct('{') => {
                let mut fields = Vec::new();
                while !self.eat('}') {
                    let key = self.name()?;
                    self.expect(':')?;
                    fields.push((key, self.value(vars)?));
                }
                Lit::Object(fields)
            }
            other => return Err(format!("expected a value, found {other}")),
        })
    }
}

/// A literal as a document writes it.
#[derive(Debug, Clone, PartialEq)]
enum Lit {
    Var(String),
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Enum(String),
    List(Vec<Lit>),
    Object(Vec<(String, Lit)>),
}

impl Lit {
    fn kind(&self) -> &'static str {
        match self {
            Lit::Var(_) => "a variable",
            Lit::Null => "null",
            Lit::Bool(_) => "a boolean",
            Lit::Int(_) => "an integer",
            Lit::Float(_) => "a float",
            Lit::Str(_) => "a string",
            Lit::Enum(_) => "an enum value",
            Lit::List(_) => "a list",
            Lit::Object(_) => "an object",
        }
    }
}

/// A value checked against an input type: a document's literal, or a request's JSON.
trait InputValue {
    /// What it is, for a refusal: `a string`, `a list`.
    fn kind(&self) -> &'static str;
    /// Whether it is a value of the scalar `name`.
    fn is_scalar(&self, name: &str) -> bool;
    /// The enum member it names, when it can name one.
    fn member(&self) -> Option<&str>;
    /// Its fields, when it is an object.
    fn fields(&self) -> Option<Vec<(&str, &Self)>>;
}

impl InputValue for Lit {
    fn kind(&self) -> &'static str {
        Lit::kind(self)
    }

    fn is_scalar(&self, name: &str) -> bool {
        match (name, self) {
            ("String" | "ID", Lit::Str(_))
            | ("Int", Lit::Int(_))
            | ("Float", Lit::Int(_) | Lit::Float(_))
            | ("Boolean", Lit::Bool(_)) => true,
            (custom, Lit::Str(_)) => !BUILT_IN.contains(&custom),
            _ => false,
        }
    }

    fn member(&self) -> Option<&str> {
        match self {
            Lit::Enum(v) => Some(v),
            _ => None,
        }
    }

    fn fields(&self) -> Option<Vec<(&str, &Self)>> {
        match self {
            Lit::Object(fields) => Some(fields.iter().map(|(k, v)| (k.as_str(), v)).collect()),
            _ => None,
        }
    }
}

impl InputValue for Value {
    fn kind(&self) -> &'static str {
        json_kind(self)
    }

    fn is_scalar(&self, name: &str) -> bool {
        match name {
            "String" | "ID" => self.is_string(),
            "Int" => self.is_i64() || self.is_u64(),
            "Float" => self.is_number(),
            "Boolean" => self.is_boolean(),
            // A custom scalar (`DateTime`) crosses as a string.
            _ => self.is_string(),
        }
    }

    fn member(&self) -> Option<&str> {
        self.as_str()
    }

    fn fields(&self) -> Option<Vec<(&str, &Self)>> {
        self.as_object()
            .map(|fields| fields.iter().map(|(k, v)| (k.as_str(), v)).collect())
    }
}

/// One selected field.
#[derive(Debug, Clone)]
struct Selection {
    name: String,
    args: Vec<(String, Lit)>,
    children: Option<Vec<Selection>>,
}

fn selection_set(c: &mut Cursor) -> Result<Vec<Selection>, String> {
    c.expect('{')?;
    let mut out = Vec::new();
    while !c.eat('}') {
        if c.peek() == Some(&Tok::Spread) {
            return Err("a fragment is refused".into());
        }
        let name = c.name()?;
        if c.at_punct(':') {
            return Err(format!("the alias `{name}` is refused"));
        }
        let mut args = Vec::new();
        if c.eat('(') {
            while !c.eat(')') {
                let arg = c.name()?;
                c.expect(':')?;
                args.push((arg, c.value(true)?));
            }
        }
        if c.at_punct('@') {
            return Err(format!("a directive on `{name}` is refused"));
        }
        let children = if c.at_punct('{') {
            Some(selection_set(c)?)
        } else {
            None
        };
        out.push(Selection {
            name,
            args,
            children,
        });
    }
    Ok(out)
}

// --- the schema ---------------------------------------------------------------------------

impl Sdl {
    /// Read the excerpt: its `type`, `input`, `enum` and `scalar` blocks, field arguments,
    /// list and non-null wrappers, and defaults.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut c = Cursor::new(text)?;
        let mut types = BTreeMap::new();
        while c.peek().is_some() {
            let keyword = c.name()?;
            let name = c.name()?;
            let def = match keyword.as_str() {
                "scalar" => TypeDef {
                    kind: Kind::Scalar,
                    fields: BTreeMap::new(),
                    values: BTreeSet::new(),
                },
                "enum" => {
                    c.expect('{')?;
                    let mut values = BTreeSet::new();
                    while !c.eat('}') {
                        values.insert(c.name()?);
                    }
                    TypeDef {
                        kind: Kind::Enum,
                        fields: BTreeMap::new(),
                        values,
                    }
                }
                "type" | "input" => {
                    if c.peek() == Some(&Tok::Name("implements".into())) {
                        return Err(format!("{name}: write it without `implements`"));
                    }
                    c.expect('{')?;
                    let mut fields = BTreeMap::new();
                    while !c.eat('}') {
                        let field = c.name()?;
                        let mut args = BTreeMap::new();
                        if c.eat('(') {
                            while !c.eat(')') {
                                let arg = c.name()?;
                                c.expect(':')?;
                                let ty = c.type_ref()?;
                                let has_default = c.eat('=');
                                if has_default {
                                    c.value(false)?;
                                }
                                args.insert(
                                    arg,
                                    Field {
                                        ty,
                                        args: BTreeMap::new(),
                                        has_default,
                                    },
                                );
                            }
                        }
                        c.expect(':')?;
                        let ty = c.type_ref()?;
                        let has_default = c.eat('=');
                        if has_default {
                            c.value(false)?;
                        }
                        if c.at_punct('@') {
                            return Err(format!("{name}.{field}: a directive is refused"));
                        }
                        fields.insert(
                            field,
                            Field {
                                ty,
                                args,
                                has_default,
                            },
                        );
                    }
                    TypeDef {
                        kind: if keyword == "type" {
                            Kind::Object
                        } else {
                            Kind::Input
                        },
                        fields,
                        values: BTreeSet::new(),
                    }
                }
                other => {
                    return Err(format!(
                        "`{other} {name}`: not a type, input, enum or scalar"
                    ))
                }
            };
            if types.insert(name.clone(), def).is_some() {
                return Err(format!("{name} is declared twice"));
            }
        }
        Ok(Self { types })
    }

    /// Every name a kept field or argument names that is neither built in nor declared: an
    /// empty list for a closed excerpt.
    pub fn unresolved(&self) -> Vec<String> {
        let mut out = BTreeSet::new();
        for (name, def) in &self.types {
            for (field, f) in &def.fields {
                for (at, ty) in std::iter::once((format!("{name}.{field}"), &f.ty)).chain(
                    f.args
                        .iter()
                        .map(|(arg, a)| (format!("{name}.{field}({arg})"), &a.ty)),
                ) {
                    if !self.known(ty.base()) {
                        out.insert(format!("{at} names {}", ty.base()));
                    }
                }
            }
        }
        out.into_iter().collect()
    }

    /// Whether the type `name` exists, built in or declared.
    pub fn has_type(&self, name: &str) -> bool {
        self.types.contains_key(name)
    }

    fn known(&self, name: &str) -> bool {
        BUILT_IN.contains(&name) || self.types.contains_key(name)
    }

    fn kind(&self, name: &str) -> Option<Kind> {
        if BUILT_IN.contains(&name) {
            return Some(Kind::Scalar);
        }
        self.types.get(name).map(|t| t.kind)
    }

    /// Check one operation document: its variable types, every argument and inline literal
    /// at every depth, every variable's use against the type it is used as, and every
    /// selected field. Returns the operation's name and variables.
    pub fn check_document(&self, document: &str) -> Result<Operation, String> {
        let mut c = Cursor::new(document)?;
        let root = match c.name()?.as_str() {
            "query" => "Query",
            "mutation" => "Mutation",
            other => return Err(format!("`{other}`: an operation is a query or a mutation")),
        };
        let name = c.name()?;
        let mut variables = Vec::new();
        if c.eat('(') {
            while !c.eat(')') {
                c.expect('$')?;
                let var = c.name()?;
                c.expect(':')?;
                let ty = c.type_ref()?;
                if c.at_punct('=') {
                    return Err(format!("${var}: a variable default is refused"));
                }
                match self.kind(ty.base()) {
                    Some(Kind::Scalar | Kind::Input | Kind::Enum) => {}
                    Some(Kind::Object) => {
                        return Err(format!("${var}: {} is not an input type", ty.base()))
                    }
                    None => return Err(format!("${var}: the type {} is unknown", ty.base())),
                }
                if variables.iter().any(|v: &VarDecl| v.name == var) {
                    return Err(format!("${var} is declared twice"));
                }
                variables.push(VarDecl { name: var, ty });
            }
        }
        let selections = selection_set(&mut c)?;
        if let Some(extra) = c.peek() {
            return Err(format!("{name}: {extra} after the operation"));
        }
        let mut uses = Vec::new();
        self.check_selections(root, &selections, &mut uses)?;
        for (var, expected, at) in &uses {
            let Some(declared) = variables.iter().find(|v| &v.name == var) else {
                return Err(format!("{at}: ${var} is used and never declared"));
            };
            if !fits(&declared.ty, expected) {
                return Err(format!(
                    "{at}: ${var} is declared {} where {expected} is",
                    declared.ty
                ));
            }
        }
        if let Some(unused) = variables
            .iter()
            .find(|v| !uses.iter().any(|(var, _, _)| var == &v.name))
        {
            return Err(format!("${} is declared and never used", unused.name));
        }
        Ok(Operation { name, variables })
    }

    fn check_selections(
        &self,
        parent: &str,
        selections: &[Selection],
        uses: &mut Vec<(String, TypeRef, String)>,
    ) -> Result<(), String> {
        let Some(def) = self.types.get(parent).filter(|d| d.kind == Kind::Object) else {
            return Err(format!("{parent} is not an object type"));
        };
        for sel in selections {
            let at = format!("{parent}.{}", sel.name);
            let Some(field) = def.fields.get(&sel.name) else {
                return Err(format!("{at}: {parent} declares no field {}", sel.name));
            };
            for (arg, lit) in &sel.args {
                let Some(declared) = field.args.get(arg) else {
                    return Err(format!("{at}({arg}): {at} declares no argument {arg}"));
                };
                self.check_lit(lit, &declared.ty, &format!("{at}({arg})"), uses)?;
            }
            for (arg, declared) in &field.args {
                let required = matches!(declared.ty, TypeRef::NonNull(_)) && !declared.has_default;
                if required && !sel.args.iter().any(|(a, _)| a == arg) {
                    return Err(format!("{at}: the argument {arg} is required"));
                }
            }
            let base = field.ty.base();
            match (self.kind(base), &sel.children) {
                (Some(Kind::Object), Some(children)) => {
                    self.check_selections(base, children, uses)?
                }
                (Some(Kind::Object), None) => {
                    return Err(format!("{at}: {base} is an object and needs a selection"))
                }
                (Some(_), Some(_)) => {
                    return Err(format!("{at}: {base} is a leaf and takes no selection"))
                }
                (Some(_), None) => {}
                (None, _) => return Err(format!("{at}: the type {base} is unknown")),
            }
        }
        Ok(())
    }

    /// Check an inline literal against `ty`, recording each variable it holds with the type
    /// it stands where.
    fn check_lit(
        &self,
        lit: &Lit,
        ty: &TypeRef,
        at: &str,
        uses: &mut Vec<(String, TypeRef, String)>,
    ) -> Result<(), String> {
        if let Lit::Var(var) = lit {
            uses.push((var.clone(), ty.clone(), at.to_string()));
            return Ok(());
        }
        match (ty, lit) {
            (TypeRef::NonNull(_), Lit::Null) => Err(format!("{at}: null where {ty} is declared")),
            (TypeRef::NonNull(inner), _) => self.check_lit(lit, inner, at, uses),
            (_, Lit::Null) => Ok(()),
            (TypeRef::List(inner), Lit::List(items)) => items
                .iter()
                .enumerate()
                .try_for_each(|(n, item)| self.check_lit(item, inner, &format!("{at}[{n}]"), uses)),
            (TypeRef::List(_), _) => Err(format!(
                "{at}: {} where the list {ty} is declared",
                lit.kind()
            )),
            (TypeRef::Named(name), _) => self.check_named(name, lit, at, |value, ty, at| {
                self.check_lit(value, ty, at, uses)
            }),
        }
    }

    /// The part of a value check a named type decides, shared by a literal and a JSON
    /// value: a scalar of its kind, a member of an enum, or an input object whose fields are
    /// each declared and each checked by `check` against its type, every required one there.
    fn check_named<'v, V: InputValue>(
        &self,
        name: &str,
        value: &'v V,
        at: &str,
        mut check: impl FnMut(&'v V, &TypeRef, &str) -> Result<(), String>,
    ) -> Result<(), String> {
        let kind = value.kind();
        match self.kind(name) {
            None => Err(format!("{at}: the type {name} is unknown")),
            Some(Kind::Object) => Err(format!("{at}: {name} is not an input type")),
            Some(Kind::Scalar) if value.is_scalar(name) => Ok(()),
            Some(Kind::Scalar) => Err(format!("{at}: {kind} where {name} is declared")),
            Some(Kind::Enum) => match value.member() {
                Some(v) if self.types[name].values.contains(v) => Ok(()),
                Some(v) => Err(format!("{at}: {v} is not a member of {name}")),
                None => Err(format!("{at}: {kind} where the enum {name} is declared")),
            },
            Some(Kind::Input) => {
                let Some(fields) = value.fields() else {
                    return Err(format!("{at}: {kind} where {name} is declared"));
                };
                let def = &self.types[name];
                for (key, field_value) in &fields {
                    let Some(field) = def.fields.get(*key) else {
                        return Err(format!("{at}.{key}: {name} declares no field {key}"));
                    };
                    check(field_value, &field.ty, &format!("{at}.{key}"))?;
                }
                for (key, field) in &def.fields {
                    let required = matches!(field.ty, TypeRef::NonNull(_)) && !field.has_default;
                    if required && !fields.iter().any(|(k, _)| k == key) {
                        return Err(format!("{at}: {name} needs its field {key}"));
                    }
                }
                Ok(())
            }
        }
    }

    /// Check the variables a request sends against the ones its document declares: every
    /// required one present and not null, none undeclared, each value of its type.
    pub fn check_variables(&self, declared: &[VarDecl], variables: &Value) -> Result<(), String> {
        let empty = serde_json::Map::new();
        let sent = match variables {
            Value::Null => &empty,
            Value::Object(sent) => sent,
            other => {
                return Err(format!(
                    "the variables are {}, not an object",
                    json_kind(other)
                ))
            }
        };
        if let Some(stray) = sent
            .keys()
            .find(|k| !declared.iter().any(|v| &v.name == *k))
        {
            return Err(format!("${stray} is sent and not declared"));
        }
        for var in declared {
            let at = format!("${}", var.name);
            match sent.get(&var.name) {
                None if matches!(var.ty, TypeRef::NonNull(_)) => {
                    return Err(format!("{at}: the required {} is not sent", var.ty))
                }
                None => {}
                Some(value) => self.check_json(value, &var.ty, &at)?,
            }
        }
        Ok(())
    }

    fn check_json(&self, value: &Value, ty: &TypeRef, at: &str) -> Result<(), String> {
        match (ty, value) {
            (TypeRef::NonNull(_), Value::Null) => Err(format!("{at}: null where {ty} is declared")),
            (TypeRef::NonNull(inner), _) => self.check_json(value, inner, at),
            (_, Value::Null) => Ok(()),
            (TypeRef::List(inner), Value::Array(items)) => items
                .iter()
                .enumerate()
                .try_for_each(|(n, item)| self.check_json(item, inner, &format!("{at}[{n}]"))),
            (TypeRef::List(_), _) => Err(format!(
                "{at}: {} where the list {ty} is declared",
                json_kind(value)
            )),
            (TypeRef::Named(name), _) => self.check_named(name, value, at, |value, ty, at| {
                self.check_json(value, ty, at)
            }),
        }
    }
}

/// Whether a variable declared `declared` may stand where `expected` is: the same type, or
/// that type with an added `!`, recursively through lists.
fn fits(declared: &TypeRef, expected: &TypeRef) -> bool {
    match (declared, expected) {
        (TypeRef::NonNull(d), TypeRef::NonNull(e)) | (TypeRef::List(d), TypeRef::List(e)) => {
            fits(d, e)
        }
        (TypeRef::NonNull(d), e) => fits(d, e),
        (TypeRef::Named(d), TypeRef::Named(e)) => d == e,
        _ => false,
    }
}

fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "an integer",
        Value::Number(_) => "a float",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EXCERPT: &str = include_str!("../schema.graphql");

    fn sdl() -> Sdl {
        Sdl::parse(EXCERPT).expect("the excerpt parses")
    }

    /// Every type a kept field or argument names is in the excerpt, so a check never meets a
    /// type it cannot judge.
    #[test]
    fn the_excerpt_is_closed() {
        assert_eq!(sdl().unresolved(), Vec::<String>::new());
    }

    /// The excerpt holds every type the operations reach, roots and inputs alike.
    #[test]
    fn the_excerpt_parses() {
        let sdl = sdl();
        for ty in [
            "Query",
            "Mutation",
            "Issue",
            "IssueFilter",
            "IssueLabelCollectionFilter",
            "ManualSort",
            "PaginationSortOrder",
            "CommentCreateInput",
            "IssueUpdateInput",
            "DateTime",
        ] {
            assert!(sdl.has_type(ty), "{ty}");
        }
    }

    /// The operation every row of the table below starts from, and its variables: an
    /// `Issues` page over a state and a label, as the plugin sends one.
    const ISSUES: &str = "query Issues($filter: IssueFilter!, $sort: [IssueSortInput!]!, \
                          $first: Int!, $after: String) {
          issues(filter: $filter, sort: $sort, first: $first, after: $after) {
            nodes { id identifier labels(first: 20) { nodes { name } } }
            pageInfo { hasNextPage endCursor }
          }
        }";

    /// One mutant per rule, each refused with the sentence it names, beside an unmutated
    /// twin that passes — so no row passes for a reason other than the one it tests.
    #[test]
    fn a_mistyped_request_is_refused() {
        let sdl = sdl();
        let documents: &[(&str, &str, &str)] = &[
            (
                "query Q($x: String) { issues(filter: {labels: {some: {name: {eq: $x}}}}) { nodes { id } } }",
                "query Q($x: String) { issues(filter: {labels: {some: {title: {eq: $x}}}}) { nodes { id } } }",
                "Query.issues(filter).labels.some.title: IssueLabelFilter declares no field title",
            ),
            (
                "query Q { issues(filter: {labels: {some: {name: {eq: \"Hold\"}}}}) { nodes { id } } }",
                "query Q { issues(filter: {labels: {some: {name: \"Hold\"}}}) { nodes { id } } }",
                "Query.issues(filter).labels.some.name: a string where StringComparator is declared",
            ),
            (
                "mutation M($id: String!, $s: String) { issueUpdate(id: $id, input: {stateId: $s}) { success } }",
                "mutation M($id: String!, $s: String) { issueUpdate(id: $id, input: {stateID: $s}) { success } }",
                "Mutation.issueUpdate(input).stateID: IssueUpdateInput declares no field stateID",
            ),
            (
                "query Q($filter: IssueFilter) { issues(filter: $filter) { nodes { id } } }",
                "query Q($filter: IssueFilters) { issues(filter: $filter) { nodes { id } } }",
                "$filter: the type IssueFilters is unknown",
            ),
            (
                "query Q($sort: [IssueSortInput!]) { issues(sort: $sort) { nodes { id } } }",
                "query Q($sort: IssueSortInput) { issues(sort: $sort) { nodes { id } } }",
                "Query.issues(sort): $sort is declared IssueSortInput where [IssueSortInput!] is",
            ),
            (
                "query Q($id: String!) { issue(id: $id) { id } }",
                "query Q($id: String!, $x: String) { issue(id: $id) { id } }",
                "$x is declared and never used",
            ),
            (
                "query Q($id: String!) { issue(id: $id) { id } }",
                "query Q { issue(id: $y) { id } }",
                "Query.issue(id): $y is used and never declared",
            ),
            (
                "query Q($f: IssueFilter) { issues(filter: $f) { nodes { id } } }",
                "query Q($f: IssueFilter) { issues(filters: $f) { nodes { id } } }",
                "Query.issues(filters): Query.issues declares no argument filters",
            ),
            (
                "query Q($id: String!) { issue(id: $id) { comments(first: 100) { nodes { id } } } }",
                "query Q($id: String!) { issue(id: $id) { comments(frist: 100) { nodes { id } } } }",
                "Issue.comments(frist): Issue.comments declares no argument frist",
            ),
            (
                "query Q { issues(sort: [{manual: {order: Ascending}}]) { nodes { id } } }",
                "query Q { issues(sort: [{manual: {order: Asc}}]) { nodes { id } } }",
                "Query.issues(sort)[0].manual.order: Asc is not a member of PaginationSortOrder",
            ),
            (
                "mutation M($t: String) { issueLabelCreate(input: {name: \"Needs Info\", teamId: $t}) { success } }",
                "mutation M($t: String) { issueLabelCreate(input: {teamId: $t}) { success } }",
                "Mutation.issueLabelCreate(input): IssueLabelCreateInput needs its field name",
            ),
            (
                "query Q($id: String!) { issue(id: $id) { id } }",
                "query Q($id: String!) { issue(id: $id) { identifer } }",
                "Issue.identifer: Issue declares no field identifer",
            ),
            (
                "query Q($id: String!) { issue(id: $id) { state { name } } }",
                "query Q($id: String!) { issue(id: $id) { state } }",
                "Issue.state: WorkflowState is an object and needs a selection",
            ),
            (
                "query Q { viewer { id } }",
                "query Q { viewer { ... on User { id } } }",
                "a fragment is refused",
            ),
            (
                "query Q { viewer { id } }",
                "query Q { issue { id } }",
                "Query.issue: the argument id is required",
            ),
            // SDL-valid, and so it passes: `IssueLabelCollectionFilter` declares `name`
            // itself. Which filter the plugin sends is pinned by `parked_vars`' own test
            // and by the fake.
            (
                "query Q($x: String) { issues(filter: {labels: {name: {eq: $x}}}) { nodes { id } } }",
                "query Q($x: String) { issues(filter: {labels: {name: {eq: $x}}}) { nodes { id } } }",
                "",
            ),
        ];
        for (good, bad, sentence) in documents {
            sdl.check_document(good)
                .unwrap_or_else(|e| panic!("the twin of {sentence:?} is refused: {e}"));
            match sdl.check_document(bad) {
                Ok(_) if sentence.is_empty() => {}
                Ok(_) => panic!("{bad} passed; want {sentence:?}"),
                Err(e) => assert_eq!(&e, sentence, "{bad}"),
            }
        }

        let declared = sdl
            .check_document(ISSUES)
            .expect("the Issues document")
            .variables;
        let base = json!({
            "filter": {"team": {"id": {"eq": "t"}}, "labels": {"some": {"name": {"eq": "Needs Info"}}}},
            "sort": [{"manual": {"order": "Ascending"}}],
            "first": 25,
            "after": null,
        });
        sdl.check_variables(&declared, &base)
            .expect("the twin passes");
        let mutate = |path: &[&str], value: Value| {
            let mut v = base.clone();
            let mut at = &mut v;
            for key in &path[..path.len() - 1] {
                at = &mut at[*key];
            }
            at[path[path.len() - 1]] = value;
            v
        };
        for (vars, sentence) in [
            (
                mutate(&["filter", "labels", "some"], json!({"title": {"eq": "x"}})),
                "$filter.labels.some.title: IssueLabelFilter declares no field title",
            ),
            (
                mutate(&["filter", "labels", "some"], json!({"name": "x"})),
                "$filter.labels.some.name: a string where StringComparator is declared",
            ),
            (
                mutate(&["sort"], json!({"manual": {"order": "Ascending"}})),
                "$sort: an object where the list [IssueSortInput!] is declared",
            ),
            (
                mutate(&["sort"], json!([{"manual": {"order": "asc"}}])),
                "$sort[0].manual.order: asc is not a member of PaginationSortOrder",
            ),
            (
                mutate(&["first"], json!("25")),
                "$first: a string where Int is declared",
            ),
            (
                mutate(&["first"], json!(null)),
                "$first: null where Int! is declared",
            ),
            (
                mutate(&["stray"], json!(1)),
                "$stray is sent and not declared",
            ),
        ] {
            assert_eq!(sdl.check_variables(&declared, &vars).unwrap_err(), sentence);
        }
        let mut missing = base.clone();
        missing.as_object_mut().unwrap().remove("filter");
        assert_eq!(
            sdl.check_variables(&declared, &missing).unwrap_err(),
            "$filter: the required IssueFilter! is not sent"
        );

        let update = sdl
            .check_document(
                "mutation IssueUpdate($id: String!, $input: IssueUpdateInput!) { \
                 issueUpdate(id: $id, input: $input) { success } }",
            )
            .unwrap()
            .variables;
        let good = json!({"id": "i", "input": {"stateId": "s", "sortOrder": 1e15}});
        sdl.check_variables(&update, &good).unwrap();
        assert_eq!(
            sdl.check_variables(
                &update,
                &json!({"id": "i", "input": {"stateId": "s", "sortOrder": "1"}})
            )
            .unwrap_err(),
            "$input.sortOrder: a string where Float is declared"
        );
        assert_eq!(
            sdl.check_variables(&update, &json!({})).unwrap_err(),
            "$id: the required String! is not sent"
        );
    }

    /// A variable may stand where its type is, or its type without the `!`, and nowhere
    /// else.
    #[test]
    fn a_variable_fits_its_own_type_or_a_nullable_one() {
        let named = |n: &str| TypeRef::Named(n.into());
        let non_null = |t: TypeRef| TypeRef::NonNull(Box::new(t));
        let list = |t: TypeRef| TypeRef::List(Box::new(t));
        assert!(fits(&named("String"), &named("String")));
        assert!(fits(&non_null(named("String")), &named("String")));
        assert!(!fits(&named("String"), &non_null(named("String"))));
        assert!(fits(
            &non_null(list(non_null(named("S")))),
            &list(non_null(named("S")))
        ));
        assert!(!fits(&list(named("S")), &list(non_null(named("S")))));
        assert!(!fits(&named("S"), &list(named("S"))));
        assert_eq!(non_null(list(non_null(named("S")))).to_string(), "[S!]!");
    }
}
