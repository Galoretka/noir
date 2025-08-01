// docs:start:from-trait
pub trait From<T> {
    fn from(input: T) -> Self;
}
// docs:end:from-trait

impl<T> From<T> for T {
    fn from(input: T) -> T {
        input
    }
}

// docs:start:into-trait
pub trait Into<T> {
    fn into(self) -> T;
}

impl<T, U> Into<T> for U
where
    T: From<U>,
{
    fn into(self) -> T {
        T::from(self)
    }
}
// docs:end:into-trait

// docs:start:from-impls
// Unsigned integers

impl From<u8> for u16 {
    fn from(value: u8) -> u16 {
        value as u16
    }
}

impl From<u8> for u32 {
    fn from(value: u8) -> u32 {
        value as u32
    }
}

impl From<u16> for u32 {
    fn from(value: u16) -> u32 {
        value as u32
    }
}

impl From<u8> for u64 {
    fn from(value: u8) -> u64 {
        value as u64
    }
}

impl From<u16> for u64 {
    fn from(value: u16) -> u64 {
        value as u64
    }
}

impl From<u32> for u64 {
    fn from(value: u32) -> u64 {
        value as u64
    }
}

impl From<u8> for u128 {
    fn from(value: u8) -> u128 {
        value as u128
    }
}

impl From<u16> for u128 {
    fn from(value: u16) -> u128 {
        value as u128
    }
}

impl From<u32> for u128 {
    fn from(value: u32) -> u128 {
        value as u128
    }
}
impl From<u64> for u128 {
    fn from(value: u64) -> u128 {
        value as u128
    }
}

impl From<u8> for Field {
    fn from(value: u8) -> Field {
        value as Field
    }
}

impl From<u16> for Field {
    fn from(value: u16) -> Field {
        value as Field
    }
}

impl From<u32> for Field {
    fn from(value: u32) -> Field {
        value as Field
    }
}
impl From<u64> for Field {
    fn from(value: u64) -> Field {
        value as Field
    }
}

impl From<u128> for Field {
    fn from(value: u128) -> Field {
        value as Field
    }
}

// Signed integers

impl From<i8> for i16 {
    fn from(value: i8) -> i16 {
        value as i16
    }
}

impl From<i8> for i32 {
    fn from(value: i8) -> i32 {
        value as i32
    }
}

impl From<i16> for i32 {
    fn from(value: i16) -> i32 {
        value as i32
    }
}

impl From<i8> for i64 {
    fn from(value: i8) -> i64 {
        value as i64
    }
}

impl From<i16> for i64 {
    fn from(value: i16) -> i64 {
        value as i64
    }
}

impl From<i32> for i64 {
    fn from(value: i32) -> i64 {
        value as i64
    }
}

// Booleans
impl From<bool> for u8 {
    fn from(value: bool) -> u8 {
        value as u8
    }
}
impl From<bool> for u16 {
    fn from(value: bool) -> u16 {
        value as u16
    }
}
impl From<bool> for u32 {
    fn from(value: bool) -> u32 {
        value as u32
    }
}
impl From<bool> for u64 {
    fn from(value: bool) -> u64 {
        value as u64
    }
}
impl From<bool> for u128 {
    fn from(value: bool) -> u128 {
        value as u128
    }
}
impl From<bool> for i8 {
    fn from(value: bool) -> i8 {
        value as i8
    }
}
impl From<bool> for i16 {
    fn from(value: bool) -> i16 {
        value as i16
    }
}
impl From<bool> for i32 {
    fn from(value: bool) -> i32 {
        value as i32
    }
}
impl From<bool> for i64 {
    fn from(value: bool) -> i64 {
        value as i64
    }
}
impl From<bool> for Field {
    fn from(value: bool) -> Field {
        value as Field
    }
}
// docs:end:from-impls

/// A generic interface for casting between primitive types,
/// equivalent of using the `as` keyword between values.
///
/// # Example
///
/// ```
/// let x: Field = 1234567890;
/// let y: u8 = x as u8;
/// let z: u8 = x.as_();
/// assert_eq(y, z);
/// ```
pub trait AsPrimitive<T> {
    /// The equivalent of doing `self as T`.
    fn as_(self) -> T;
}

#[generate_as_primitive_impls]
comptime fn generate_as_primitive_impls(_: FunctionDefinition) -> Quoted {
    let types = [
        quote { bool },
        quote { u8 },
        quote { u16 },
        quote { u32 },
        quote { u64 },
        quote { u128 },
        quote { i8 },
        quote { i16 },
        quote { i32 },
        quote { i64 },
    ];

    let mut impls = &[];
    for type1 in types {
        for type2 in types {
            let body = if type1 == type2 {
                quote { self }
            } else if type1 == quote { bool } {
                quote { self != 0 }
            } else {
                quote { self as $type1 }
            };

            impls = impls.push_back(
                quote {
                impl AsPrimitive<$type1> for $type2 {
                    fn as_(self) -> $type1 {
                        $body
                    }
                }
            },
            );
        }
    }

    let u_types =
        [quote { bool }, quote { u8 }, quote { u16 }, quote { u32 }, quote { u64 }, quote { u128 }];

    for type2 in u_types {
        let body = quote { self as Field };

        impls = impls.push_back(
            quote {
                impl AsPrimitive<Field> for $type2 {
                    fn as_(self) -> Field {
                        $body
                    }
                }
            },
        );
    }

    for type1 in u_types {
        let body = if type1 == quote { bool } {
            quote { self != 0 }
        } else {
            quote { self as $type1 }
        };

        impls = impls.push_back(
            quote {
                impl AsPrimitive<$type1> for Field {
                    fn as_(self) -> $type1 {
                        $body
                    }
                }
            },
        );
    }

    impls.join(quote {})
}
