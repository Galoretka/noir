use noirc_errors::Location;
use noirc_frontend::shared::Visibility;
use noirc_frontend::{
    ast::{NoirTrait, Param, Pattern, TraitItem},
    token::{Attributes, Keyword, Token},
};

use super::{Formatter, function::FunctionToFormat};

impl Formatter<'_> {
    pub(super) fn format_trait(&mut self, noir_trait: NoirTrait) {
        self.format_secondary_attributes(noir_trait.attributes);
        self.write_indentation();
        self.format_item_visibility(noir_trait.visibility);
        self.write_keyword(Keyword::Trait);
        self.write_space();
        self.write_identifier(noir_trait.name);
        self.format_generics(noir_trait.generics);

        if noir_trait.is_alias {
            self.write_space();
            self.write_token(Token::Assign);
        }

        if !noir_trait.bounds.is_empty() {
            self.skip_comments_and_whitespace();

            if !noir_trait.is_alias {
                self.write_token(Token::Colon);
            }

            self.write_space();
            self.format_trait_bounds(noir_trait.bounds);
        }

        if !noir_trait.where_clause.is_empty() {
            self.format_where_clause(noir_trait.where_clause, true);
        }

        // aliases have ';' in lieu of '{ items }'
        if noir_trait.is_alias {
            self.write_semicolon();
            return;
        }

        self.write_space();
        self.write_left_brace();
        if noir_trait.items.is_empty() {
            self.increase_indentation();
            self.skip_comments_and_whitespace();
            self.decrease_indentation();
        } else {
            self.increase_indentation();
            self.write_line();

            for (index, documented_item) in noir_trait.items.into_iter().enumerate() {
                if index > 0 {
                    self.write_line();
                }

                let doc_comments = documented_item.doc_comments;
                let item = documented_item.item;
                if !doc_comments.is_empty() {
                    self.format_outer_doc_comments();
                }
                self.format_trait_item(item);
            }

            self.skip_comments_and_whitespace();
            self.decrease_indentation();
            self.write_line();
            self.write_indentation();
        }
        self.write_right_brace();
    }

    fn format_trait_item(&mut self, item: TraitItem) {
        match item {
            TraitItem::Function {
                is_unconstrained: _,
                visibility,
                is_comptime: _,
                name,
                generics,
                parameters,
                return_type,
                where_clause,
                body,
            } => {
                let parameters = parameters
                    .into_iter()
                    .map(|(name, typ)| Param {
                        visibility: Visibility::Private,
                        pattern: Pattern::Identifier(name),
                        typ,
                        location: Location::dummy(), // Doesn't matter
                    })
                    .collect();

                let func = FunctionToFormat {
                    attributes: Attributes::empty(),
                    visibility,
                    name,
                    generics,
                    parameters,
                    return_type,
                    return_visibility: Visibility::Private,
                    where_clause,
                    body,
                    skip_visibility: true,
                };
                self.format_function_impl(func);
            }
            TraitItem::Constant { name, typ } => {
                let pattern = Pattern::Identifier(name);
                let chunks = self.chunk_formatter().format_let_or_global(
                    Keyword::Let,
                    pattern,
                    typ,
                    None,
                    Vec::new(), // Attributes
                );
                self.write_indentation();
                self.format_chunk_group(chunks);
            }
            TraitItem::Type { name, bounds } => {
                self.write_indentation();
                self.write_keyword(Keyword::Type);
                self.write_space();
                self.write_identifier(name);
                if !bounds.is_empty() {
                    self.write_token(Token::Colon);
                    self.write_space();
                    self.format_trait_bounds(bounds);
                }
                self.write_semicolon();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::assert_format;

    #[test]
    fn format_empty_trait() {
        let src = " mod moo { /// Comment
        pub  trait Foo {  } }";
        let expected = "mod moo {
    /// Comment
    pub trait Foo {}
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_empty_trait_with_generics() {
        let src = " mod moo { trait Foo < A, B, > {  } }";
        let expected = "mod moo {
    trait Foo<A, B> {}
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_parents() {
        let src = " mod moo { trait Foo : Bar  +  Baz {  } }";
        let expected = "mod moo {
    trait Foo: Bar + Baz {}
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_where_clause() {
        let src = " mod moo { trait Foo < T >  where  T : Bar {  } }";
        let expected = "mod moo {
    trait Foo<T>
    where
        T: Bar,
    {}
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_type() {
        let src = " mod moo { trait Foo { 
    /// hello
            type X;
         } }";
        let expected = "mod moo {
    trait Foo {
        /// hello
        type X;
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_type_and_bounds() {
        let src = " mod moo { trait Foo { 
    /// hello
            type X : A  +  B  ;
         } }";
        let expected = "mod moo {
    trait Foo {
        /// hello
        type X: A + B;
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_constant_no_value() {
        let src = " mod moo { trait Foo { 
            let  x  : i32 ;
         } }";
        let expected = "mod moo {
    trait Foo {
        let x: i32;
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_function_without_body() {
        let src = " mod moo { trait Foo { 
    /// hello 
            fn  foo ( );
         } }";
        let expected = "mod moo {
    trait Foo {
        /// hello
        fn foo();
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_function_with_body() {
        let src = " mod moo { trait Foo { 
    /// hello 
            fn  foo ( ) { 1 }
         } }";
        let expected = "mod moo {
    trait Foo {
        /// hello
        fn foo() {
            1
        }
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_function_with_params() {
        let src = " mod moo { trait Foo { 
    /// hello 
            fn  foo ( x : i32 , y : Field );
         } }";
        let expected = "mod moo {
    trait Foo {
        /// hello
        fn foo(x: i32, y: Field);
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_function_with_where_clause() {
        let src = " mod moo { trait Foo { 
            fn  foo<T> () where  T : Bar;
         } }";
        let expected = "mod moo {
    trait Foo {
        fn foo<T>()
        where
            T: Bar;
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_function_with_multiple_where_clauses() {
        let src = " mod moo { trait Foo { 
            fn  foo<T> () where  A: B, C: D;
         } }";
        let expected = "mod moo {
    trait Foo {
        fn foo<T>()
        where
            A: B,
            C: D;
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_trait_with_function_with_visibility() {
        let src = " mod moo { trait Foo { 
    /// hello 
            pub  fn  foo ( ) { 1 }
         } }";
        let expected = "mod moo {
    trait Foo {
        /// hello
        fn foo() {
            1
        }
    }
}
";
        assert_format(src, expected);
    }

    #[test]
    fn format_multiple_traits() {
        let src = " trait Foo {}

        trait Bar {}";
        let expected = "trait Foo {}

trait Bar {}
";
        assert_format(src, expected);
    }
}
