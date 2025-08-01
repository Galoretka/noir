use std::fmt::Display;

use acvm::FieldElement;
use noirc_errors::{Position, Span, Spanned};
use noirc_frontend::token::IntType;

#[derive(Debug)]
pub(crate) struct SpannedToken(Spanned<Token>);

impl SpannedToken {
    pub(crate) fn new(token: Token, span: Span) -> SpannedToken {
        SpannedToken(Spanned::from(span, token))
    }

    pub(crate) fn span(&self) -> Span {
        self.0.span()
    }

    pub(crate) fn token(&self) -> &Token {
        &self.0.contents
    }

    pub(crate) fn into_token(self) -> Token {
        self.0.contents
    }
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Token {
    Ident(String),
    Int(FieldElement),
    Str(String),
    ByteStr(String),
    Keyword(Keyword),
    IntType(IntType),
    /// =
    Assign,
    /// (
    LeftParen,
    /// )
    RightParen,
    /// {
    LeftBrace,
    /// }
    RightBrace,
    /// [
    LeftBracket,
    /// ]
    RightBracket,
    /// ,
    Comma,
    /// :
    Colon,
    /// ;
    Semicolon,
    /// ->
    Arrow,
    /// ==
    Equal,
    /// !=
    NotEqual,
    /// &
    Ampersand,
    /// -
    Dash,
    Eof,
}

impl Token {
    pub(super) fn into_single_span(self, position: Position) -> SpannedToken {
        self.into_span(position, position)
    }

    pub(super) fn into_span(self, start: Position, end: Position) -> SpannedToken {
        SpannedToken(Spanned::from_position(start, end, self))
    }
}

impl Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Token::Ident(ident) => write!(f, "{ident}"),
            Token::Int(int) => write!(f, "{int}"),
            Token::Str(string) => write!(f, "{string:?}"),
            Token::ByteStr(string) => write!(f, "{string:?}"),
            Token::Keyword(keyword) => write!(f, "{keyword}"),
            Token::IntType(int_type) => write!(f, "{int_type}"),
            Token::Assign => write!(f, "="),
            Token::LeftParen => write!(f, "("),
            Token::RightParen => write!(f, ")"),
            Token::LeftBrace => write!(f, "{{"),
            Token::RightBrace => write!(f, "}}"),
            Token::LeftBracket => write!(f, "["),
            Token::RightBracket => write!(f, "]"),
            Token::Comma => write!(f, ","),
            Token::Colon => write!(f, ":"),
            Token::Semicolon => write!(f, ";"),
            Token::Arrow => write!(f, "->"),
            Token::Equal => write!(f, "=="),
            Token::NotEqual => write!(f, "!="),
            Token::Ampersand => write!(f, "&"),
            Token::Dash => write!(f, "-"),
            Token::Eof => write!(f, "(end of stream)"),
        }
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Keyword {
    Acir,
    Add,
    Allocate,
    And,
    ArrayGet,
    ArraySet,
    As,
    At,
    Bits,
    Bool,
    Brillig,
    Call,
    Cast,
    Constrain,
    Data,
    DecRc,
    Div,
    Inline,
    InlineAlways,
    Else,
    EnableSideEffects,
    Eq,
    Field,
    Fold,
    Fn,
    Function,
    If,
    Impure,
    IncRc,
    Index,
    Jmp,
    Jmpif,
    Load,
    Lt,
    MakeArray,
    MaxBitSize,
    Minus,
    Mod,
    Mul,
    Mut,
    Nop,
    NoPredicates,
    Not,
    Of,
    Or,
    PredicatePure,
    Pure,
    RangeCheck,
    Return,
    Shl,
    Shr,
    Store,
    Sub,
    Then,
    To,
    Truncate,
    UncheckedAdd,
    UncheckedSub,
    UncheckedMul,
    Unreachable,
    Value,
    Xor,
}

impl Keyword {
    pub(crate) fn lookup_keyword(word: &str) -> Option<Token> {
        let keyword = match word {
            "acir" => Keyword::Acir,
            "add" => Keyword::Add,
            "allocate" => Keyword::Allocate,
            "and" => Keyword::And,
            "array_get" => Keyword::ArrayGet,
            "array_set" => Keyword::ArraySet,
            "as" => Keyword::As,
            "at" => Keyword::At,
            "bits" => Keyword::Bits,
            "bool" => Keyword::Bool,
            "brillig" => Keyword::Brillig,
            "call" => Keyword::Call,
            "cast" => Keyword::Cast,
            "constrain" => Keyword::Constrain,
            "data" => Keyword::Data,
            "dec_rc" => Keyword::DecRc,
            "div" => Keyword::Div,
            "else" => Keyword::Else,
            "enable_side_effects" => Keyword::EnableSideEffects,
            "eq" => Keyword::Eq,
            "if" => Keyword::If,
            "impure" => Keyword::Impure,
            "inline" => Keyword::Inline,
            "inline_always" => Keyword::InlineAlways,
            "Field" => Keyword::Field,
            "fold" => Keyword::Fold,
            "fn" => Keyword::Fn,
            "function" => Keyword::Function,
            "inc_rc" => Keyword::IncRc,
            "index" => Keyword::Index,
            "jmp" => Keyword::Jmp,
            "jmpif" => Keyword::Jmpif,
            "load" => Keyword::Load,
            "lt" => Keyword::Lt,
            "make_array" => Keyword::MakeArray,
            "max_bit_size" => Keyword::MaxBitSize,
            "minus" => Keyword::Minus,
            "mod" => Keyword::Mod,
            "mul" => Keyword::Mul,
            "mut" => Keyword::Mut,
            "nop" => Keyword::Nop,
            "no_predicates" => Keyword::NoPredicates,
            "not" => Keyword::Not,
            "of" => Keyword::Of,
            "or" => Keyword::Or,
            "predicate_pure" => Keyword::PredicatePure,
            "pure" => Keyword::Pure,
            "range_check" => Keyword::RangeCheck,
            "return" => Keyword::Return,
            "shl" => Keyword::Shl,
            "shr" => Keyword::Shr,
            "store" => Keyword::Store,
            "sub" => Keyword::Sub,
            "then" => Keyword::Then,
            "to" => Keyword::To,
            "truncate" => Keyword::Truncate,
            "unchecked_add" => Keyword::UncheckedAdd,
            "unchecked_sub" => Keyword::UncheckedSub,
            "unchecked_mul" => Keyword::UncheckedMul,
            "unreachable" => Keyword::Unreachable,
            "value" => Keyword::Value,
            "xor" => Keyword::Xor,
            _ => return None,
        };
        Some(Token::Keyword(keyword))
    }
}

impl Display for Keyword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Keyword::Acir => write!(f, "acir"),
            Keyword::Add => write!(f, "add"),
            Keyword::Allocate => write!(f, "allocate"),
            Keyword::And => write!(f, "and"),
            Keyword::ArrayGet => write!(f, "array_get"),
            Keyword::ArraySet => write!(f, "array_set"),
            Keyword::As => write!(f, "as"),
            Keyword::At => write!(f, "at"),
            Keyword::Bits => write!(f, "bits"),
            Keyword::Bool => write!(f, "bool"),
            Keyword::Brillig => write!(f, "brillig"),
            Keyword::Call => write!(f, "call"),
            Keyword::Cast => write!(f, "cast"),
            Keyword::Constrain => write!(f, "constrain"),
            Keyword::Data => write!(f, "data"),
            Keyword::DecRc => write!(f, "dec_rc"),
            Keyword::Div => write!(f, "div"),
            Keyword::Else => write!(f, "else"),
            Keyword::EnableSideEffects => write!(f, "enable_side_effects"),
            Keyword::Eq => write!(f, "eq"),
            Keyword::Field => write!(f, "Field"),
            Keyword::Fold => write!(f, "fold"),
            Keyword::Fn => write!(f, "fn"),
            Keyword::Function => write!(f, "function"),
            Keyword::If => write!(f, "if"),
            Keyword::Impure => write!(f, "impure"),
            Keyword::IncRc => write!(f, "inc_rc"),
            Keyword::Index => write!(f, "index"),
            Keyword::Inline => write!(f, "inline"),
            Keyword::InlineAlways => write!(f, "inline_always"),
            Keyword::Jmp => write!(f, "jmp"),
            Keyword::Jmpif => write!(f, "jmpif"),
            Keyword::Load => write!(f, "load"),
            Keyword::Lt => write!(f, "lt"),
            Keyword::MakeArray => write!(f, "make_array"),
            Keyword::MaxBitSize => write!(f, "max_bit_size"),
            Keyword::Minus => write!(f, "minus"),
            Keyword::Mod => write!(f, "mod"),
            Keyword::Mul => write!(f, "mul"),
            Keyword::Mut => write!(f, "mut"),
            Keyword::Nop => write!(f, "nop"),
            Keyword::NoPredicates => write!(f, "no_predicates"),
            Keyword::Not => write!(f, "not"),
            Keyword::Of => write!(f, "of"),
            Keyword::Or => write!(f, "or"),
            Keyword::PredicatePure => write!(f, "predicate_pure"),
            Keyword::Pure => write!(f, "pure"),
            Keyword::RangeCheck => write!(f, "range_check"),
            Keyword::Return => write!(f, "return"),
            Keyword::Shl => write!(f, "shl"),
            Keyword::Shr => write!(f, "shr"),
            Keyword::Store => write!(f, "store"),
            Keyword::Sub => write!(f, "sub"),
            Keyword::Then => write!(f, "then"),
            Keyword::To => write!(f, "to"),
            Keyword::Truncate => write!(f, "truncate"),
            Keyword::UncheckedAdd => write!(f, "unchecked_add"),
            Keyword::UncheckedSub => write!(f, "unchecked_sub"),
            Keyword::UncheckedMul => write!(f, "unchecked_mul"),
            Keyword::Unreachable => write!(f, "unreachable"),
            Keyword::Value => write!(f, "value"),
            Keyword::Xor => write!(f, "xor"),
        }
    }
}
