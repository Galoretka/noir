use std::fmt::Display;

use acvm::FieldElement;
use acvm::acir::AcirField;
use iter_extended::vecmap;
use noirc_errors::{Located, Location, Span};

use super::{
    BinaryOpKind, BlockExpression, ConstructorExpression, Expression, ExpressionKind,
    GenericTypeArgs, IndexExpression, InfixExpression, ItemVisibility, MemberAccessExpression,
    MethodCallExpression, UnresolvedType,
};
use crate::ast::UnresolvedTypeData;
use crate::elaborator::types::SELF_TYPE_NAME;
use crate::graph::CrateId;
use crate::node_interner::{
    InternedExpressionKind, InternedPattern, InternedStatementKind, NodeInterner,
};
use crate::parser::{ParserError, ParserErrorReason};
use crate::token::{LocatedToken, SecondaryAttribute, Token};

/// This is used when an identifier fails to parse in the parser.
/// Instead of failing the parse, we can often recover using this
/// as the default value instead. Further passes like name resolution
/// should also check for this ident to avoid issuing multiple errors
/// for an identifier that already failed to parse.
pub const ERROR_IDENT: &str = "$error";

/// This is used to represent an UnresolvedTypeData::Unspecified in a Path
pub const WILDCARD_TYPE: &str = "_";

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Statement {
    pub kind: StatementKind,
    pub location: Location,
}

/// Ast node for statements in noir. Statements are always within a block { }
/// of some kind and are terminated via a Semicolon, except if the statement
/// ends in a block, such as a Statement::Expression containing an if expression.
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum StatementKind {
    Let(LetStatement),
    Expression(Expression),
    Assign(AssignStatement),
    For(ForLoopStatement),
    Loop(Expression, Location /* loop keyword location */),
    While(WhileStatement),
    Break,
    Continue,
    /// This statement should be executed at compile-time
    Comptime(Box<Statement>),
    // This is an expression with a trailing semi-colon
    Semi(Expression),
    // This is an interned StatementKind during comptime code.
    // The actual StatementKind can be retrieved with a NodeInterner.
    Interned(InternedStatementKind),
    // This statement is the result of a recovered parse error.
    // To avoid issuing multiple errors in later steps, it should
    // be skipped in any future analysis if possible.
    Error,
}

impl Statement {
    pub fn add_semicolon(
        mut self,
        semi: Option<Token>,
        location: Location,
        last_statement_in_block: bool,
        emit_error: &mut dyn FnMut(ParserError),
    ) -> Self {
        self.kind = self.kind.add_semicolon(semi, location, last_statement_in_block, emit_error);
        self
    }

    /// Returns the innermost location that gives this statement its type.
    pub fn type_location(&self) -> Location {
        match &self.kind {
            StatementKind::Expression(expression) => expression.type_location(),
            StatementKind::Comptime(statement) => statement.type_location(),
            StatementKind::Let(..)
            | StatementKind::Assign(..)
            | StatementKind::For(..)
            | StatementKind::Loop(..)
            | StatementKind::While(..)
            | StatementKind::Break
            | StatementKind::Continue
            | StatementKind::Semi(..)
            | StatementKind::Interned(..)
            | StatementKind::Error => self.location,
        }
    }
}

impl StatementKind {
    pub fn add_semicolon(
        self,
        semi: Option<Token>,
        location: Location,
        last_statement_in_block: bool,
        emit_error: &mut dyn FnMut(ParserError),
    ) -> Self {
        let missing_semicolon =
            ParserError::with_reason(ParserErrorReason::MissingSeparatingSemi, location);

        match self {
            StatementKind::Let(_) => {
                // To match rust, a let statement always requires a semicolon, even at the end of a block
                if semi.is_none() {
                    let reason = ParserErrorReason::MissingSemicolonAfterLet;
                    emit_error(ParserError::with_reason(reason, location));
                }
                self
            }
            StatementKind::Assign(_)
            | StatementKind::Semi(_)
            | StatementKind::Break
            | StatementKind::Continue
            | StatementKind::Error => {
                // These statements can omit the semicolon if they are the last statement in a block
                if !last_statement_in_block && semi.is_none() {
                    emit_error(missing_semicolon);
                }
                self
            }
            StatementKind::Comptime(mut statement) => {
                *statement =
                    statement.add_semicolon(semi, location, last_statement_in_block, emit_error);
                StatementKind::Comptime(statement)
            }
            // A semicolon on a for loop, loop or while is optional and does nothing
            StatementKind::For(_) | StatementKind::Loop(..) | StatementKind::While(..) => self,

            // No semicolon needed for a resolved statement
            StatementKind::Interned(_) => self,

            StatementKind::Expression(expr) => {
                match (&expr.kind, semi, last_statement_in_block) {
                    // Semicolons are optional for these expressions
                    (ExpressionKind::Block(_), semi, _)
                    | (ExpressionKind::Unsafe(..), semi, _)
                    | (ExpressionKind::Interned(..), semi, _)
                    | (ExpressionKind::InternedStatement(..), semi, _)
                    | (ExpressionKind::Match(..), semi, _)
                    | (ExpressionKind::If(_), semi, _) => {
                        if semi.is_some() {
                            StatementKind::Semi(expr)
                        } else {
                            StatementKind::Expression(expr)
                        }
                    }
                    (_, None, false) => {
                        emit_error(missing_semicolon);
                        StatementKind::Expression(expr)
                    }
                    (_, Some(_), _) => StatementKind::Semi(expr),
                    (_, None, true) => StatementKind::Expression(expr),
                }
            }
        }
    }
}

impl StatementKind {
    pub fn new_let(
        pattern: Pattern,
        r#type: UnresolvedType,
        expression: Expression,
        attributes: Vec<SecondaryAttribute>,
    ) -> StatementKind {
        StatementKind::Let(LetStatement {
            pattern,
            r#type,
            expression,
            comptime: false,
            is_global_let: false,
            attributes,
        })
    }
}

#[derive(Eq, Debug, Clone)]
pub struct Ident(Located<String>);

impl Ident {
    /// Gets the underlying identifier without its location.
    pub fn identifier(&self) -> &str {
        &self.0.contents
    }
}

impl PartialEq<Ident> for Ident {
    fn eq(&self, other: &Ident) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialOrd for Ident {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ident {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl PartialEq<str> for Ident {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl std::hash::Hash for Ident {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl Display for Ident {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_str().fmt(f)
    }
}

impl From<Located<String>> for Ident {
    fn from(a: Located<String>) -> Ident {
        Ident(a)
    }
}

impl From<String> for Ident {
    fn from(a: String) -> Ident {
        Located::from(Location::dummy(), a).into()
    }
}
impl From<&str> for Ident {
    fn from(a: &str) -> Ident {
        Ident::from(a.to_owned())
    }
}

impl From<LocatedToken> for Ident {
    fn from(lt: LocatedToken) -> Ident {
        let located_str = Located::from(lt.location(), lt.token().to_string());
        Ident(located_str)
    }
}

impl From<Ident> for Expression {
    fn from(i: Ident) -> Expression {
        let location = i.location();
        let kind = ExpressionKind::Variable(Path::plain(vec![PathSegment::from(i)], location));
        Expression { location, kind }
    }
}

impl Ident {
    pub fn new(text: String, location: Location) -> Ident {
        Ident(Located::from(location, text))
    }

    pub fn is_self_type_name(&self) -> bool {
        self.as_str() == SELF_TYPE_NAME
    }

    pub fn is_empty(&self) -> bool {
        self.as_str().is_empty()
    }

    pub fn location(&self) -> Location {
        self.0.location()
    }

    pub fn span(&self) -> Span {
        self.0.span()
    }

    pub fn as_str(&self) -> &str {
        &self.0.contents
    }

    pub fn as_string(&self) -> &String {
        &self.0.contents
    }

    pub fn into_string(self) -> String {
        self.0.contents
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ModuleDeclaration {
    pub visibility: ItemVisibility,
    pub ident: Ident,
    pub outer_attributes: Vec<SecondaryAttribute>,
    pub has_semicolon: bool,
}

impl std::fmt::Display for ModuleDeclaration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mod {}", self.ident)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ImportStatement {
    pub visibility: ItemVisibility,
    pub path: Path,
    pub alias: Option<Ident>,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub enum PathKind {
    Crate,
    Dep,
    Plain,
    Super,
    /// This path is a Crate or Dep path which always points to the given crate
    Resolved(CrateId),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct UseTree {
    pub prefix: Path,
    pub kind: UseTreeKind,
    pub location: Location,
}

impl Display for UseTree {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.prefix)?;

        if !self.prefix.segments.is_empty() {
            write!(f, "::")?;
        }

        match &self.kind {
            UseTreeKind::Path(name, alias) => {
                write!(f, "{name}")?;

                if let Some(alias) = alias {
                    write!(f, " as {alias}")?;
                }

                Ok(())
            }
            UseTreeKind::List(trees) => {
                write!(f, "{{")?;
                let tree = vecmap(trees, ToString::to_string).join(", ");
                write!(f, "{tree}}}")
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum UseTreeKind {
    Path(Ident, Option<Ident>),
    List(Vec<UseTree>),
}

impl UseTree {
    pub fn desugar(self, root: Option<Path>, visibility: ItemVisibility) -> Vec<ImportStatement> {
        let prefix = if let Some(mut root) = root {
            root.segments.extend(self.prefix.segments);
            root
        } else {
            self.prefix
        };

        match self.kind {
            UseTreeKind::Path(name, alias) => {
                // Desugar `use foo::{self}` to `use foo`
                let path = if name.as_str() == "self" { prefix } else { prefix.join(name) };
                vec![ImportStatement { visibility, path, alias }]
            }
            UseTreeKind::List(trees) => {
                let trees = trees.into_iter();
                trees.flat_map(|tree| tree.desugar(Some(prefix.clone()), visibility)).collect()
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct UnsafeExpression {
    pub block: BlockExpression,
    pub unsafe_keyword_location: Location,
}

/// A special kind of path in the form `<MyType as Trait>::ident`.
/// Note that this path must consist of exactly two segments.
///
/// An AsTraitPath may be used in either a type context where `ident`
/// refers to an associated type of a particular impl, or in a value
/// context where `ident` may refer to an associated constant or a
/// function within the impl.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct AsTraitPath {
    pub typ: UnresolvedType,
    pub trait_path: Path,
    pub trait_generics: GenericTypeArgs,
    pub impl_item: Ident,
}

/// A special kind of path in the form `Type::ident::<turbofish>`
/// Unlike normal paths, the type here can be a primitive type or
/// interned type.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct TypePath {
    pub typ: UnresolvedType,
    pub item: Ident,
    pub turbofish: Option<GenericTypeArgs>,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct Path {
    pub segments: Vec<PathSegment>,
    pub kind: PathKind,
    pub location: Location,
    // The location of `kind` (this is the same as `location` for plain kinds)
    pub kind_location: Location,
}

impl Path {
    pub fn plain(segments: Vec<PathSegment>, location: Location) -> Self {
        Self { segments, location, kind: PathKind::Plain, kind_location: location }
    }

    pub fn pop(&mut self) -> PathSegment {
        self.segments.pop().unwrap()
    }

    fn join(mut self, ident: Ident) -> Path {
        self.segments.push(PathSegment::from(ident));
        self
    }

    /// Construct a PathKind::Plain from this single
    pub fn from_single(name: String, location: Location) -> Path {
        let segment = Ident::from(Located::from(location, name));
        Path::from_ident(segment)
    }

    pub fn from_ident(name: Ident) -> Path {
        let location = name.location();
        Path::plain(vec![PathSegment::from(name)], location)
    }

    pub fn span(&self) -> Span {
        self.location.span
    }

    pub fn last_segment(&self) -> PathSegment {
        assert!(!self.segments.is_empty());
        self.segments.last().unwrap().clone()
    }

    pub fn last_ident(&self) -> Ident {
        self.last_segment().ident
    }

    pub fn first_name(&self) -> Option<&str> {
        self.segments.first().map(|segment| segment.ident.as_str())
    }

    pub fn last_name(&self) -> &str {
        assert!(!self.segments.is_empty());
        self.segments.last().unwrap().ident.as_str()
    }

    pub fn is_ident(&self) -> bool {
        self.kind == PathKind::Plain
            && self.segments.len() == 1
            && self.segments.first().unwrap().generics.is_none()
    }

    pub fn as_ident(&self) -> Option<&Ident> {
        if !self.is_ident() {
            return None;
        }
        self.segments.first().map(|segment| &segment.ident)
    }

    pub(crate) fn is_wildcard(&self) -> bool {
        if let Some(ident) = self.as_ident() { ident.as_str() == WILDCARD_TYPE } else { false }
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty() && self.kind == PathKind::Plain
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct PathSegment {
    pub ident: Ident,
    pub generics: Option<Vec<UnresolvedType>>,
    pub location: Location,
}

impl PathSegment {
    /// Returns the span where turbofish happen. For example:
    ///
    /// ```noir
    ///    foo::<T>
    ///       ~^^^^
    /// ```
    ///
    /// Returns an empty span at the end of `foo` if there's no turbofish.
    pub fn turbofish_span(&self) -> Span {
        if self.ident.location().file == self.location.file {
            Span::from(self.ident.span().end()..self.location.span.end())
        } else {
            self.location.span
        }
    }

    pub fn turbofish_location(&self) -> Location {
        Location::new(self.turbofish_span(), self.location.file)
    }
}

impl From<Ident> for PathSegment {
    fn from(ident: Ident) -> PathSegment {
        let location = ident.location();
        PathSegment { ident, generics: None, location }
    }
}

impl Display for PathSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.ident.fmt(f)?;

        if let Some(generics) = &self.generics {
            let generics = vecmap(generics, ToString::to_string);
            write!(f, "::<{}>", generics.join(", "))?;
        }

        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct LetStatement {
    pub pattern: Pattern,
    pub r#type: UnresolvedType,
    pub expression: Expression,
    pub attributes: Vec<SecondaryAttribute>,

    // True if this should only be run during compile-time
    pub comptime: bool,
    pub is_global_let: bool,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct AssignStatement {
    pub lvalue: LValue,
    pub expression: Expression,
}

/// Represents an Ast form that can be assigned to
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum LValue {
    Path(Path),
    MemberAccess { object: Box<LValue>, field_name: Ident, location: Location },
    Index { array: Box<LValue>, index: Expression, location: Location },
    Dereference(Box<LValue>, Location),
    Interned(InternedExpressionKind, Location),
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum Pattern {
    Identifier(Ident),
    Mutable(Box<Pattern>, Location, /*is_synthesized*/ bool),
    Tuple(Vec<Pattern>, Location),
    Struct(Path, Vec<(Ident, Pattern)>, Location),
    Parenthesized(Box<Pattern>, Location),
    Interned(InternedPattern, Location),
}

impl Pattern {
    pub fn location(&self) -> Location {
        match self {
            Pattern::Identifier(ident) => ident.location(),
            Pattern::Mutable(_, location, _)
            | Pattern::Tuple(_, location)
            | Pattern::Struct(_, _, location)
            | Pattern::Parenthesized(_, location)
            | Pattern::Interned(_, location) => *location,
        }
    }

    pub fn span(&self) -> Span {
        self.location().span
    }

    pub fn name_ident(&self) -> &Ident {
        match self {
            Pattern::Identifier(name_ident) => name_ident,
            Pattern::Mutable(pattern, ..) => pattern.name_ident(),
            _ => panic!("Only the Identifier or Mutable patterns can return a name"),
        }
    }

    pub(crate) fn try_as_expression(&self, interner: &NodeInterner) -> Option<Expression> {
        match self {
            Pattern::Identifier(ident) => Some(Expression {
                kind: ExpressionKind::Variable(Path::from_ident(ident.clone())),
                location: ident.location(),
            }),
            Pattern::Mutable(_, _, _) => None,
            Pattern::Tuple(patterns, location) => {
                let mut expressions = Vec::new();
                for pattern in patterns {
                    expressions.push(pattern.try_as_expression(interner)?);
                }
                Some(Expression { kind: ExpressionKind::Tuple(expressions), location: *location })
            }
            Pattern::Struct(path, patterns, location) => {
                let mut fields = Vec::new();
                for (field, pattern) in patterns {
                    let expression = pattern.try_as_expression(interner)?;
                    fields.push((field.clone(), expression));
                }
                Some(Expression {
                    kind: ExpressionKind::Constructor(Box::new(ConstructorExpression {
                        typ: UnresolvedType::from_path(path.clone()),
                        fields,
                    })),
                    location: *location,
                })
            }
            Pattern::Parenthesized(pattern, _) => pattern.try_as_expression(interner),
            Pattern::Interned(id, _) => interner.get_pattern(*id).try_as_expression(interner),
        }
    }
}

impl LValue {
    pub fn as_expression(&self) -> Expression {
        let kind = match self {
            LValue::Path(path) => ExpressionKind::Variable(path.clone()),
            LValue::MemberAccess { object, field_name, location: _ } => {
                ExpressionKind::MemberAccess(Box::new(MemberAccessExpression {
                    lhs: object.as_expression(),
                    rhs: field_name.clone(),
                }))
            }
            LValue::Index { array, index, location: _ } => {
                ExpressionKind::Index(Box::new(IndexExpression {
                    collection: array.as_expression(),
                    index: index.clone(),
                }))
            }
            LValue::Dereference(lvalue, _span) => {
                ExpressionKind::Prefix(Box::new(crate::ast::PrefixExpression {
                    operator: crate::ast::UnaryOp::Dereference { implicitly_added: false },
                    rhs: lvalue.as_expression(),
                }))
            }
            LValue::Interned(id, _) => ExpressionKind::Interned(*id),
        };
        Expression::new(kind, self.location())
    }

    pub fn from_expression(expr: Expression) -> Option<LValue> {
        LValue::from_expression_kind(expr.kind, expr.location)
    }

    pub fn from_expression_kind(expr: ExpressionKind, location: Location) -> Option<LValue> {
        match expr {
            ExpressionKind::Variable(path) => Some(LValue::Path(path)),
            ExpressionKind::MemberAccess(member_access) => Some(LValue::MemberAccess {
                object: Box::new(LValue::from_expression(member_access.lhs)?),
                field_name: member_access.rhs,
                location,
            }),
            ExpressionKind::Index(index) => Some(LValue::Index {
                array: Box::new(LValue::from_expression(index.collection)?),
                index: index.index,
                location,
            }),
            ExpressionKind::Prefix(prefix) => {
                if matches!(
                    prefix.operator,
                    crate::ast::UnaryOp::Dereference { implicitly_added: false }
                ) {
                    Some(LValue::Dereference(
                        Box::new(LValue::from_expression(prefix.rhs)?),
                        location,
                    ))
                } else {
                    None
                }
            }
            ExpressionKind::Parenthesized(expr) => LValue::from_expression(*expr),
            ExpressionKind::Interned(id) => Some(LValue::Interned(id, location)),
            _ => None,
        }
    }

    pub fn location(&self) -> Location {
        match self {
            LValue::Path(path) => path.location,
            LValue::MemberAccess { location, .. }
            | LValue::Index { location, .. }
            | LValue::Dereference(_, location)
            | LValue::Interned(_, location) => *location,
        }
    }

    pub fn span(&self) -> Span {
        self.location().span
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ForBounds {
    pub start: Expression,
    pub end: Expression,
    pub inclusive: bool,
}

impl ForBounds {
    /// Create a half-open range bounded inclusively below and exclusively above (`start..end`),
    /// desugaring `start..=end` into `start..end+1` if necessary.
    ///
    /// Returns the `start` and `end` expressions.
    pub(crate) fn into_half_open(self) -> (Expression, Expression) {
        let end = if self.inclusive {
            let end_location = self.end.location;
            let end = ExpressionKind::Infix(Box::new(InfixExpression {
                lhs: self.end,
                operator: Located::from(end_location, BinaryOpKind::Add),
                rhs: Expression::new(
                    ExpressionKind::integer(FieldElement::from(1u32), None),
                    end_location,
                ),
            }));
            Expression::new(end, end_location)
        } else {
            self.end
        };

        (self.start, end)
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ForRange {
    Range(ForBounds),
    Array(Expression),
}

impl ForRange {
    /// Create a half-open range, bounded inclusively below and exclusively above.
    pub fn range(start: Expression, end: Expression) -> Self {
        Self::Range(ForBounds { start, end, inclusive: false })
    }

    /// Create a 'for' expression taking care of desugaring a 'for e in array' loop
    /// into the following if needed:
    ///
    /// ```text
    /// {
    ///     let fresh1 = array;
    ///     for fresh2 in 0 .. std::array::len(fresh1) {
    ///         let elem = fresh1[fresh2];
    ///         ...
    ///     }
    /// }
    /// ````
    pub(crate) fn into_for(
        self,
        identifier: Ident,
        block: Expression,
        for_loop_location: Location,
    ) -> Statement {
        // Counter used to generate unique names when desugaring
        // code in the parser requires the creation of fresh variables.
        let mut unique_name_counter: u32 = 0;

        match self {
            ForRange::Range(..) => {
                unreachable!()
            }
            ForRange::Array(array) => {
                let array_location = array.location;
                let start_range = ExpressionKind::integer(FieldElement::zero(), None);
                let start_range = Expression::new(start_range, array_location);

                let next_unique_id = unique_name_counter;
                unique_name_counter += 1;
                let array_name = format!("$i{next_unique_id}");
                let array_location = array.location;
                let array_ident = Ident::new(array_name, array_location);

                // let fresh1 = array;
                let let_array = Statement {
                    kind: StatementKind::new_let(
                        Pattern::Identifier(array_ident.clone()),
                        UnresolvedTypeData::Unspecified.with_dummy_location(),
                        array,
                        vec![],
                    ),
                    location: array_location,
                };

                // array.len()
                let segments = vec![PathSegment::from(array_ident)];
                let array_ident = ExpressionKind::Variable(Path::plain(segments, array_location));

                let end_range = ExpressionKind::MethodCall(Box::new(MethodCallExpression {
                    object: Expression::new(array_ident.clone(), array_location),
                    method_name: Ident::new("len".to_string(), array_location),
                    generics: None,
                    is_macro_call: false,
                    arguments: vec![],
                }));
                let end_range = Expression::new(end_range, array_location);

                let next_unique_id = unique_name_counter;
                let index_name = format!("$i{next_unique_id}");
                let fresh_identifier = Ident::new(index_name.clone(), array_location);

                // array[i]
                let segments = vec![PathSegment::from(Ident::new(index_name, array_location))];
                let index_ident = ExpressionKind::Variable(Path::plain(segments, array_location));

                let loop_element = ExpressionKind::Index(Box::new(IndexExpression {
                    collection: Expression::new(array_ident, array_location),
                    index: Expression::new(index_ident, array_location),
                }));

                // let elem = array[i];
                let let_elem = Statement {
                    kind: StatementKind::new_let(
                        Pattern::Identifier(identifier),
                        UnresolvedTypeData::Unspecified.with_dummy_location(),
                        Expression::new(loop_element, array_location),
                        vec![],
                    ),
                    location: array_location,
                };

                let block_location = block.location;
                let new_block = BlockExpression {
                    statements: vec![
                        let_elem,
                        Statement {
                            kind: StatementKind::Expression(block),
                            location: block_location,
                        },
                    ],
                };
                let new_block = Expression::new(ExpressionKind::Block(new_block), block_location);
                let for_loop = Statement {
                    kind: StatementKind::For(ForLoopStatement {
                        identifier: fresh_identifier,
                        range: ForRange::range(start_range, end_range),
                        block: new_block,
                        location: for_loop_location,
                    }),
                    location: for_loop_location,
                };

                let block = ExpressionKind::Block(BlockExpression {
                    statements: vec![let_array, for_loop],
                });
                Statement {
                    kind: StatementKind::Expression(Expression::new(block, for_loop_location)),
                    location: for_loop_location,
                }
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct ForLoopStatement {
    pub identifier: Ident,
    pub range: ForRange,
    pub block: Expression,
    pub location: Location,
}

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct WhileStatement {
    pub condition: Expression,
    pub body: Expression,
    pub while_keyword_location: Location,
}

impl Display for StatementKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StatementKind::Let(let_statement) => let_statement.fmt(f),
            StatementKind::Expression(expression) => expression.fmt(f),
            StatementKind::Assign(assign) => assign.fmt(f),
            StatementKind::For(for_loop) => for_loop.fmt(f),
            StatementKind::Loop(block, _) => write!(f, "loop {block}"),
            StatementKind::While(while_) => {
                write!(f, "while {} {}", while_.condition, while_.body)
            }
            StatementKind::Break => write!(f, "break"),
            StatementKind::Continue => write!(f, "continue"),
            StatementKind::Comptime(statement) => write!(f, "comptime {}", statement.kind),
            StatementKind::Semi(semi) => write!(f, "{semi};"),
            StatementKind::Interned(_) => write!(f, "(resolved);"),
            StatementKind::Error => write!(f, "Error"),
        }
    }
}

impl Display for LetStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if matches!(&self.r#type.typ, UnresolvedTypeData::Unspecified) {
            write!(f, "let {} = {}", self.pattern, self.expression)
        } else {
            write!(f, "let {}: {} = {}", self.pattern, self.r#type, self.expression)
        }
    }
}

impl Display for AssignStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} = {}", self.lvalue, self.expression)
    }
}

impl Display for LValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LValue::Path(ident) => ident.fmt(f),
            LValue::MemberAccess { object, field_name, location: _ } => {
                write!(f, "{object}.{field_name}")
            }
            LValue::Index { array, index, location: _ } => write!(f, "{array}[{index}]"),
            LValue::Dereference(lvalue, _span) => write!(f, "*{lvalue}"),
            LValue::Interned(_, _) => write!(f, "?Interned"),
        }
    }
}

impl Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let segments = vecmap(&self.segments, ToString::to_string);
        if self.kind == PathKind::Plain {
            write!(f, "{}", segments.join("::"))
        } else {
            write!(f, "{}::{}", self.kind, segments.join("::"))
        }
    }
}

impl Display for PathKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathKind::Crate => write!(f, "crate"),
            PathKind::Dep => write!(f, "dep"),
            PathKind::Super => write!(f, "super"),
            PathKind::Plain => write!(f, "plain"),
            PathKind::Resolved(_) => write!(f, "$crate"),
        }
    }
}

impl Display for ImportStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "use {}", self.path)?;
        if let Some(alias) = &self.alias {
            write!(f, " as {alias}")?;
        }
        Ok(())
    }
}

impl Display for Pattern {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Pattern::Identifier(name) => name.fmt(f),
            Pattern::Mutable(name, _, _) => write!(f, "mut {name}"),
            Pattern::Tuple(fields, _) => {
                let fields = vecmap(fields, ToString::to_string);
                if fields.len() == 1 {
                    write!(f, "({},)", fields[0])
                } else {
                    write!(f, "({})", fields.join(", "))
                }
            }
            Pattern::Struct(typename, fields, _) => {
                let fields = vecmap(fields, |(name, pattern)| format!("{name}: {pattern}"));
                write!(f, "{} {{ {} }}", typename, fields.join(", "))
            }
            Pattern::Parenthesized(pattern, _) => {
                write!(f, "({pattern})")
            }
            Pattern::Interned(_, _) => {
                write!(f, "?Interned")
            }
        }
    }
}

impl Display for ForLoopStatement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let range = match &self.range {
            ForRange::Range(bounds) => {
                format!(
                    "{}{}{}",
                    bounds.start,
                    if bounds.inclusive { "..=" } else { ".." },
                    bounds.end
                )
            }
            ForRange::Array(expr) => expr.to_string(),
        };

        write!(f, "for {} in {range} {}", self.identifier, self.block)
    }
}
