use crate::{
    BinaryTypeOperator,
    ast::{GenericTypeArgs, UnresolvedType, UnresolvedTypeData, UnresolvedTypeExpression},
    parser::{ParserError, labels::ParsingRuleLabel},
    token::Token,
};

use acvm::acir::{AcirField, FieldElement};
use noirc_errors::Location;

use super::{Parser, parse_many::separated_by_comma_until_right_paren};

impl Parser<'_> {
    /// TypeExpression= AddOrSubtractTypeExpression
    pub(crate) fn parse_type_expression(
        &mut self,
    ) -> Result<UnresolvedTypeExpression, ParserError> {
        match self.parse_add_or_subtract_type_expression() {
            Some(type_expr) => Ok(type_expr),
            None => self.expected_type_expression_after_this(),
        }
    }

    /// AddOrSubtractTypeExpression
    ///     = MultiplyOrDivideOrModuloTypeExpression ( ( '+' | '-' ) MultiplyOrDivideOrModuloTypeExpression )*
    fn parse_add_or_subtract_type_expression(&mut self) -> Option<UnresolvedTypeExpression> {
        let start_location = self.current_token_location;
        let lhs = self.parse_multiply_or_divide_or_modulo_type_expression()?;
        Some(self.parse_add_or_subtract_type_expression_after_lhs(lhs, start_location))
    }

    fn parse_add_or_subtract_type_expression_after_lhs(
        &mut self,
        mut lhs: UnresolvedTypeExpression,
        start_location: Location,
    ) -> UnresolvedTypeExpression {
        loop {
            let operator = if self.eat(Token::Plus) {
                BinaryTypeOperator::Addition
            } else if self.eat(Token::Minus) {
                BinaryTypeOperator::Subtraction
            } else {
                break;
            };

            match self.parse_multiply_or_divide_or_modulo_type_expression() {
                Some(rhs) => {
                    let location = self.location_since(start_location);
                    lhs = UnresolvedTypeExpression::BinaryOperation(
                        Box::new(lhs),
                        operator,
                        Box::new(rhs),
                        location,
                    );
                }
                None => {
                    self.push_expected_expression();
                }
            }
        }

        lhs
    }

    /// MultiplyOrDivideOrModuloTypeExpression
    ///     = TermTypeExpression ( ( '*' | '/' | '%' ) TermTypeExpression )*
    fn parse_multiply_or_divide_or_modulo_type_expression(
        &mut self,
    ) -> Option<UnresolvedTypeExpression> {
        let start_location = self.current_token_location;
        let lhs = self.parse_term_type_expression()?;
        Some(self.parse_multiply_or_divide_or_modulo_type_expression_after_lhs(lhs, start_location))
    }

    fn parse_multiply_or_divide_or_modulo_type_expression_after_lhs(
        &mut self,
        mut lhs: UnresolvedTypeExpression,
        start_location: Location,
    ) -> UnresolvedTypeExpression {
        loop {
            let operator = if self.eat(Token::Star) {
                BinaryTypeOperator::Multiplication
            } else if self.eat(Token::Slash) {
                BinaryTypeOperator::Division
            } else if self.eat(Token::Percent) {
                BinaryTypeOperator::Modulo
            } else {
                break;
            };

            match self.parse_term_type_expression() {
                Some(rhs) => {
                    let location = self.location_since(start_location);
                    lhs = UnresolvedTypeExpression::BinaryOperation(
                        Box::new(lhs),
                        operator,
                        Box::new(rhs),
                        location,
                    );
                }
                None => {
                    self.push_expected_expression();
                    break;
                }
            }
        }

        lhs
    }

    /// TermTypeExpression
    ///    = '- TermTypeExpression
    ///    | AtomTypeExpression
    fn parse_term_type_expression(&mut self) -> Option<UnresolvedTypeExpression> {
        let start_location = self.current_token_location;
        if self.eat(Token::Minus) {
            return match self.parse_term_type_expression() {
                Some(rhs) => {
                    let lhs = UnresolvedTypeExpression::Constant(
                        FieldElement::zero(),
                        None,
                        start_location,
                    );
                    let op = BinaryTypeOperator::Subtraction;
                    let location = self.location_since(start_location);
                    Some(UnresolvedTypeExpression::BinaryOperation(
                        Box::new(lhs),
                        op,
                        Box::new(rhs),
                        location,
                    ))
                }
                None => {
                    self.push_expected_expression();
                    None
                }
            };
        }

        self.parse_atom_type_expression()
    }

    /// AtomTypeExpression
    ///     = ConstantTypeExpression
    ///     | VariableTypeExpression
    ///     | AsTraitPathTypeExpression
    ///     | ParenthesizedTypeExpression
    fn parse_atom_type_expression(&mut self) -> Option<UnresolvedTypeExpression> {
        if let Some(type_expr) = self.parse_constant_type_expression() {
            return Some(type_expr);
        }

        if let Some(type_expr) = self.parse_variable_type_expression() {
            return Some(type_expr);
        }

        if let Some(as_trait_path) = self.parse_as_trait_path() {
            return Some(UnresolvedTypeExpression::AsTraitPath(Box::new(as_trait_path)));
        }

        if let Some(type_expr) = self.parse_parenthesized_type_expression() {
            return Some(type_expr);
        }

        None
    }

    /// ConstantTypeExpression = int
    fn parse_constant_type_expression(&mut self) -> Option<UnresolvedTypeExpression> {
        let (int, suffix) = self.eat_int()?;
        Some(UnresolvedTypeExpression::Constant(int, suffix, self.previous_token_location))
    }

    /// VariableTypeExpression = Path
    fn parse_variable_type_expression(&mut self) -> Option<UnresolvedTypeExpression> {
        let path = self.parse_path()?;
        Some(UnresolvedTypeExpression::Variable(path))
    }

    /// ParenthesizedTypeExpression = '(' TypeExpression ')'
    fn parse_parenthesized_type_expression(&mut self) -> Option<UnresolvedTypeExpression> {
        // Make sure not to parse `()` as a parenthesized expression
        if self.at(Token::LeftParen) && !self.next_is(Token::RightParen) {
            self.bump();
            match self.parse_type_expression() {
                Ok(type_expr) => {
                    self.eat_or_error(Token::RightParen);
                    Some(type_expr)
                }
                Err(error) => {
                    self.errors.push(error);
                    self.eat_right_paren();
                    None
                }
            }
        } else {
            None
        }
    }

    /// TypeOrTypeExpression = Type | TypeExpression
    pub(crate) fn parse_type_or_type_expression(&mut self) -> Option<UnresolvedType> {
        let typ = self.parse_add_or_subtract_type_or_type_expression()?;
        let span = typ.location;

        // If we end up with a Variable type expression, make it a Named type (they are equivalent),
        // but for testing purposes and simplicity we default to types instead of type expressions.
        Some(
            if let UnresolvedTypeData::Expression(UnresolvedTypeExpression::Variable(path)) =
                typ.typ
            {
                UnresolvedType {
                    typ: UnresolvedTypeData::Named(path, GenericTypeArgs::default(), false),
                    location: span,
                }
            } else {
                typ
            },
        )
    }

    fn parse_add_or_subtract_type_or_type_expression(&mut self) -> Option<UnresolvedType> {
        let start_location = self.current_token_location;
        let lhs = self.parse_multiply_or_divide_or_modulo_type_or_type_expression()?;

        // If lhs is a type then no operator can follow, so we stop right away
        if !type_is_type_expr(&lhs) {
            return Some(lhs);
        }

        let lhs = type_to_type_expr(lhs).unwrap();
        let lhs = self.parse_add_or_subtract_type_expression_after_lhs(lhs, start_location);
        Some(type_expr_to_type(lhs, self.location_since(start_location)))
    }

    fn parse_multiply_or_divide_or_modulo_type_or_type_expression(
        &mut self,
    ) -> Option<UnresolvedType> {
        let start_location = self.current_token_location;
        let lhs = self.parse_term_type_or_type_expression()?;

        // If lhs is a type then no operator can follow, so we stop right away
        if !type_is_type_expr(&lhs) {
            return Some(lhs);
        }

        let lhs = type_to_type_expr(lhs).unwrap();
        let lhs =
            self.parse_multiply_or_divide_or_modulo_type_expression_after_lhs(lhs, start_location);
        Some(type_expr_to_type(lhs, self.location_since(start_location)))
    }

    fn parse_term_type_or_type_expression(&mut self) -> Option<UnresolvedType> {
        let start_location = self.current_token_location;
        if self.eat(Token::Minus) {
            // If we ate '-' what follows must be a type expression, never a type
            return match self.parse_term_type_expression() {
                Some(rhs) => {
                    let lhs = UnresolvedTypeExpression::Constant(
                        FieldElement::zero(),
                        None,
                        start_location,
                    );
                    let op = BinaryTypeOperator::Subtraction;
                    let location = self.location_since(start_location);
                    let type_expr = UnresolvedTypeExpression::BinaryOperation(
                        Box::new(lhs),
                        op,
                        Box::new(rhs),
                        location,
                    );
                    let typ = UnresolvedTypeData::Expression(type_expr);
                    Some(UnresolvedType { typ, location })
                }
                None => {
                    self.push_expected_expression();
                    None
                }
            };
        }

        self.parse_atom_type_or_type_expression()
    }

    fn parse_atom_type_or_type_expression(&mut self) -> Option<UnresolvedType> {
        let start_location = self.current_token_location;

        if let Some(path) = self.parse_path() {
            let generics = self.parse_generic_type_args();
            let typ = UnresolvedTypeData::Named(path, generics, false);
            let location = self.location_since(start_location);
            return Some(UnresolvedType { typ, location });
        }

        if let Some(type_expr) = self.parse_constant_type_expression() {
            let typ = UnresolvedTypeData::Expression(type_expr);
            let location = self.location_since(start_location);
            return Some(UnresolvedType { typ, location });
        }

        if let Some(typ) = self.parse_parenthesized_type_or_type_expression() {
            return Some(typ);
        }

        self.parse_type()
    }

    fn parse_parenthesized_type_or_type_expression(&mut self) -> Option<UnresolvedType> {
        let start_location = self.current_token_location;

        if !self.eat_left_paren() {
            return None;
        }

        if self.eat_right_paren() {
            return Some(UnresolvedType {
                typ: UnresolvedTypeData::Unit,
                location: self.location_since(start_location),
            });
        }

        let Some(typ) = self.parse_type_or_type_expression() else {
            self.expected_label(ParsingRuleLabel::TypeOrTypeExpression);
            return None;
        };

        // If what we just parsed is a type expression then this must be a parenthesized type
        // expression (there's no such thing as a tuple of type expressions)
        if let UnresolvedTypeData::Expression(type_expr) = typ.typ {
            self.eat_or_error(Token::RightParen);
            return Some(UnresolvedType {
                typ: UnresolvedTypeData::Expression(type_expr),
                location: typ.location,
            });
        }

        if self.eat_right_paren() {
            return Some(UnresolvedType {
                typ: UnresolvedTypeData::Parenthesized(Box::new(typ)),
                location: self.location_since(start_location),
            });
        }

        let comma_after_first_type = self.eat_comma();
        let second_type_location = self.current_token_location;

        let mut types = self.parse_many(
            "tuple items",
            separated_by_comma_until_right_paren(),
            Self::parse_type_in_list,
        );

        if !types.is_empty() && !comma_after_first_type {
            self.expected_token_separating_items(Token::Comma, "tuple items", second_type_location);
        }

        types.insert(0, typ);

        Some(UnresolvedType {
            typ: UnresolvedTypeData::Tuple(types),
            location: self.location_since(start_location),
        })
    }

    fn expected_type_expression_after_this(
        &mut self,
    ) -> Result<UnresolvedTypeExpression, ParserError> {
        Err(ParserError::expected_label(
            ParsingRuleLabel::TypeExpression,
            self.token.token().clone(),
            self.current_token_location,
        ))
    }
}

fn type_to_type_expr(typ: UnresolvedType) -> Option<UnresolvedTypeExpression> {
    match typ.typ {
        UnresolvedTypeData::Named(var, generics, _) => {
            if generics.is_empty() {
                Some(UnresolvedTypeExpression::Variable(var))
            } else {
                None
            }
        }
        UnresolvedTypeData::Expression(type_expr) => Some(type_expr),
        _ => None,
    }
}

fn type_is_type_expr(typ: &UnresolvedType) -> bool {
    match &typ.typ {
        UnresolvedTypeData::Named(_, generics, _) => generics.is_empty(),
        UnresolvedTypeData::Expression(..) => true,
        _ => false,
    }
}

fn type_expr_to_type(lhs: UnresolvedTypeExpression, location: Location) -> UnresolvedType {
    let lhs = UnresolvedTypeData::Expression(lhs);
    UnresolvedType { typ: lhs, location }
}

#[cfg(test)]
mod tests {
    use core::panic;

    use crate::{
        BinaryTypeOperator,
        ast::{UnresolvedType, UnresolvedTypeData, UnresolvedTypeExpression},
        parser::{
            Parser, ParserErrorReason,
            parser::tests::{
                expect_no_errors, get_single_error_reason, get_source_with_error_span,
            },
        },
        token::Token,
    };

    fn parse_type_expression_no_errors(src: &str) -> UnresolvedTypeExpression {
        let mut parser = Parser::for_str_with_dummy_file(src);
        let expr = parser.parse_type_expression().unwrap();
        expect_no_errors(&parser.errors);
        expr
    }

    fn parse_type_or_type_expression_no_errors(src: &str) -> UnresolvedType {
        let mut parser = Parser::for_str_with_dummy_file(src);
        let typ = parser.parse_type_or_type_expression().unwrap();
        expect_no_errors(&parser.errors);
        typ
    }

    #[test]
    fn parses_constant_type_expression() {
        let src = "42";
        let expr = parse_type_expression_no_errors(src);
        let UnresolvedTypeExpression::Constant(n, _, _) = expr else {
            panic!("Expected constant");
        };
        assert_eq!(n, 42_u32.into());
    }

    #[test]
    fn parses_variable_type_expression() {
        let src = "foo::bar";
        let expr = parse_type_expression_no_errors(src);
        let UnresolvedTypeExpression::Variable(path) = expr else {
            panic!("Expected path");
        };
        assert_eq!(path.to_string(), "foo::bar");
    }

    #[test]
    fn parses_binary_type_expression() {
        let src = "1 + 2 * 3 + 4";
        let expr = parse_type_expression_no_errors(src);
        let UnresolvedTypeExpression::BinaryOperation(lhs, operator, rhs, _) = expr else {
            panic!("Expected binary operation");
        };
        assert_eq!(lhs.to_string(), "(1 + (2 * 3))");
        assert_eq!(operator, BinaryTypeOperator::Addition);
        assert_eq!(rhs.to_string(), "4");
    }

    #[test]
    fn parses_parenthesized_type_expression() {
        let src = "(N)";
        let expr = parse_type_expression_no_errors(src);
        let UnresolvedTypeExpression::Variable(path) = expr else {
            panic!("Expected variable");
        };
        assert_eq!(path.to_string(), "N");
    }

    #[test]
    fn parses_minus_type_expression() {
        let src = "-N";
        let expr = parse_type_expression_no_errors(src);
        assert_eq!(expr.to_string(), "(0 - N)");
    }

    #[test]
    fn parses_as_trait_path_type_expression() {
        let src = "<Type as Trait>::AssociatedType";
        let typ = parse_type_expression_no_errors(src);
        let UnresolvedTypeExpression::AsTraitPath(as_trait_path) = typ else {
            panic!("Expected AsTraitPath");
        };
        assert_eq!(as_trait_path.typ.to_string(), "Type");
        assert_eq!(as_trait_path.trait_path.to_string(), "Trait");
        assert_eq!(as_trait_path.impl_item.to_string(), "AssociatedType");
    }

    #[test]
    fn parse_type_or_type_expression_constant() {
        let src = "42";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Expression(expr) = typ.typ else {
            panic!("Expected expression");
        };
        let UnresolvedTypeExpression::Constant(n, _, _) = expr else {
            panic!("Expected constant");
        };
        assert_eq!(n, 42_u32.into());
    }

    #[test]
    fn parse_type_or_type_expression_variable() {
        let src = "foo::Bar";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Named(path, generics, _) = typ.typ else {
            panic!("Expected named type");
        };
        assert_eq!(path.to_string(), "foo::Bar");
        assert!(generics.is_empty());
    }

    #[test]
    fn parses_type_or_type_expression_binary() {
        let src = "1 + 2 * 3 + 4";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Expression(expr) = typ.typ else {
            panic!("Expected expression");
        };
        let UnresolvedTypeExpression::BinaryOperation(lhs, operator, rhs, _) = expr else {
            panic!("Expected binary operation");
        };
        assert_eq!(lhs.to_string(), "(1 + (2 * 3))");
        assert_eq!(operator, BinaryTypeOperator::Addition);
        assert_eq!(rhs.to_string(), "4");
    }

    #[test]
    fn parses_type_or_type_expression_minus() {
        let src = "-N";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Expression(expr) = typ.typ else {
            panic!("Expected expression");
        };
        assert_eq!(expr.to_string(), "(0 - N)");
    }

    #[test]
    fn parses_type_or_type_expression_unit() {
        let src = "()";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Unit = typ.typ else {
            panic!("Expected unit type");
        };
    }

    #[test]
    fn parses_type_or_type_expression_parenthesized_type() {
        let src = "(Field)";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Parenthesized(typ) = typ.typ else {
            panic!("Expected parenthesized type");
        };
        assert_eq!(typ.typ.to_string(), "Field");
    }

    #[test]
    fn parses_type_or_type_expression_parenthesized_constant() {
        let src = "(1)";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Expression(expr) = typ.typ else {
            panic!("Expected expression type");
        };
        assert_eq!(expr.to_string(), "1");
    }

    #[test]
    fn parses_type_or_type_expression_tuple_type() {
        let src = "(Field, bool)";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Tuple(types) = typ.typ else {
            panic!("Expected tuple type");
        };
        assert_eq!(types[0].typ.to_string(), "Field");
        assert_eq!(types[1].typ.to_string(), "bool");
    }

    #[test]
    fn parses_type_or_type_expression_tuple_type_missing_comma() {
        let src = "
        (Field bool)
               ^^^^
        ";
        let (src, span) = get_source_with_error_span(src);
        let mut parser = Parser::for_str_with_dummy_file(&src);

        let typ = parser.parse_type_or_type_expression().unwrap();

        let reason = get_single_error_reason(&parser.errors, span);
        let ParserErrorReason::ExpectedTokenSeparatingTwoItems { token, items } = reason else {
            panic!("Expected a different error");
        };
        assert_eq!(token, &Token::Comma);
        assert_eq!(*items, "tuple items");

        let UnresolvedTypeData::Tuple(types) = typ.typ else {
            panic!("Expected tuple type");
        };
        assert_eq!(types[0].typ.to_string(), "Field");
        assert_eq!(types[1].typ.to_string(), "bool");
    }

    #[test]
    fn parses_type_or_type_expression_tuple_type_single_element() {
        let src = "(Field,)";
        let mut parser = Parser::for_str_with_dummy_file(src);
        let typ = parser.parse_type_or_type_expression().unwrap();
        expect_no_errors(&parser.errors);
        let UnresolvedTypeData::Tuple(types) = typ.typ else {
            panic!("Expected tuple type");
        };
        assert_eq!(types.len(), 1);
        assert_eq!(types[0].typ.to_string(), "Field");
    }

    #[test]
    fn parses_type_or_type_expression_var_minus_one() {
        let src = "N - 1";
        let typ = parse_type_or_type_expression_no_errors(src);
        let UnresolvedTypeData::Expression(expr) = typ.typ else {
            panic!("Expected expression type");
        };
        assert_eq!(expr.to_string(), "(N - 1)");
    }
}
