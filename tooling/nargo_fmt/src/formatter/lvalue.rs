use noirc_frontend::{ast::LValue, token::Token};

use super::Formatter;
use crate::chunks::ChunkGroup;

impl Formatter<'_> {
    pub(super) fn format_lvalue(&mut self, lvalue: LValue) {
        // Parenthesized l-values exist but are not represented in the AST
        while let Token::LeftParen = self.token {
            self.write_left_paren();
        }

        match lvalue {
            LValue::Path(path) => self.format_path(path),
            LValue::MemberAccess { object, field_name, location: _ } => {
                self.format_lvalue(*object);
                self.write_token(Token::Dot);
                self.write_identifier_or_integer(field_name);
            }
            LValue::Index { array, index, location: _ } => {
                self.format_lvalue(*array);
                self.write_left_bracket();
                let mut group = ChunkGroup::new();
                self.chunk_formatter().format_expression(index, &mut group);
                self.format_chunk_group(group);
                self.write_right_bracket();
            }
            LValue::Dereference(lvalue, _span) => {
                self.write_token(Token::Star);
                self.format_lvalue(*lvalue);
            }
            LValue::Interned(..) => {
                unreachable!("Should not be present in the AST")
            }
        }

        self.skip_comments_and_whitespace();

        // Parenthesized l-values exist but are not represented in the AST
        while let Token::RightParen = self.token {
            self.write_right_paren();
        }
    }
}
