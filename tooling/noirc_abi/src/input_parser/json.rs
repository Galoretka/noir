use super::{
    InputValue, field_to_signed_hex, parse_integer_to_signed, parse_str_to_field,
    parse_str_to_signed,
};
use crate::{Abi, AbiType, MAIN_RETURN_NAME, errors::InputParserError};
use acvm::{AcirField, FieldElement};
use iter_extended::{try_btree_map, try_vecmap};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub fn parse_json(
    input_string: &str,
    abi: &Abi,
) -> Result<BTreeMap<String, InputValue>, InputParserError> {
    // Parse input.json into a BTreeMap.
    let data: BTreeMap<String, JsonTypes> = serde_json::from_str(input_string)?;

    // Convert arguments to field elements.
    let mut parsed_inputs = try_btree_map(abi.to_btree_map(), |(arg_name, abi_type)| {
        // Check that json contains a value for each argument in the ABI.
        let value = data
            .get(&arg_name)
            .ok_or_else(|| InputParserError::MissingArgument(arg_name.clone()))?;

        InputValue::try_from_json(value.clone(), &abi_type, &arg_name)
            .map(|input_value| (arg_name, input_value))
    })?;

    // If the json file also includes a return value then we parse it as well.
    // This isn't required as the prover calculates the return value itself.
    if let (Some(return_type), Some(json_return_value)) =
        (&abi.return_type, data.get(MAIN_RETURN_NAME))
    {
        let return_value = InputValue::try_from_json(
            json_return_value.clone(),
            &return_type.abi_type,
            MAIN_RETURN_NAME,
        )?;
        parsed_inputs.insert(MAIN_RETURN_NAME.to_owned(), return_value);
    }

    Ok(parsed_inputs)
}

pub fn serialize_to_json(
    input_map: &BTreeMap<String, InputValue>,
    abi: &Abi,
) -> Result<String, InputParserError> {
    let mut json_map = try_btree_map(abi.to_btree_map(), |(key, param_type)| {
        JsonTypes::try_from_input_value(&input_map[&key], &param_type)
            .map(|value| (key.to_owned(), value))
    })?;

    if let (Some(return_type), Some(return_value)) =
        (&abi.return_type, input_map.get(MAIN_RETURN_NAME))
    {
        let return_value = JsonTypes::try_from_input_value(return_value, &return_type.abi_type)?;
        json_map.insert(MAIN_RETURN_NAME.to_owned(), return_value);
    }

    let json_string = serde_json::to_string(&json_map)?;

    Ok(json_string)
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(untagged)]
pub enum JsonTypes {
    // This is most likely going to be a hex string
    // But it is possible to support UTF-8
    String(String),
    // Just a regular integer, that can fit in 64 bits.
    //
    // The JSON spec does not specify any limit on the size of integer number types,
    // however we restrict the allowable size. Values which do not fit in a i64 should be passed
    // as a string.
    Integer(i64),
    // Simple boolean flag
    Bool(bool),
    // Array of JsonTypes
    Array(Vec<JsonTypes>),
    // Struct of JsonTypes
    Table(BTreeMap<String, JsonTypes>),
}

impl JsonTypes {
    pub fn try_from_input_value(
        value: &InputValue,
        abi_type: &AbiType,
    ) -> Result<JsonTypes, InputParserError> {
        let json_value = match (value, abi_type) {
            (InputValue::Field(f), AbiType::Integer { sign: crate::Sign::Signed, width }) => {
                JsonTypes::String(field_to_signed_hex(*f, *width))
            }
            (InputValue::Field(f), AbiType::Field | AbiType::Integer { .. }) => {
                JsonTypes::String(Self::format_field_string(*f))
            }
            (InputValue::Field(f), AbiType::Boolean) => JsonTypes::Bool(f.is_one()),

            (InputValue::Vec(vector), AbiType::Array { typ, .. }) => {
                let array =
                    try_vecmap(vector, |value| JsonTypes::try_from_input_value(value, typ))?;
                JsonTypes::Array(array)
            }

            (InputValue::String(s), AbiType::String { .. }) => JsonTypes::String(s.to_string()),

            (InputValue::Struct(map), AbiType::Struct { fields, .. }) => {
                let map_with_json_types = try_btree_map(fields, |(key, field_type)| {
                    JsonTypes::try_from_input_value(&map[key], field_type)
                        .map(|json_value| (key.to_owned(), json_value))
                })?;
                JsonTypes::Table(map_with_json_types)
            }

            (InputValue::Vec(vector), AbiType::Tuple { fields }) => {
                let fields = try_vecmap(vector.iter().zip(fields), |(value, typ)| {
                    JsonTypes::try_from_input_value(value, typ)
                })?;
                JsonTypes::Array(fields)
            }

            _ => {
                return Err(InputParserError::AbiTypeMismatch(
                    format!("{value:?}"),
                    abi_type.clone(),
                ));
            }
        };
        Ok(json_value)
    }

    /// This trims any leading zeroes.
    /// A singular '0' will be prepended as well if the trimmed string has an odd length.
    /// A hex string's length needs to be even to decode into bytes, as two digits correspond to
    /// one byte.
    fn format_field_string(field: FieldElement) -> String {
        if field.is_zero() {
            return "0x00".to_owned();
        }
        let mut trimmed_field = field.to_hex().trim_start_matches('0').to_owned();
        if trimmed_field.len() % 2 != 0 {
            trimmed_field = "0".to_owned() + &trimmed_field;
        }
        "0x".to_owned() + &trimmed_field
    }
}

impl InputValue {
    pub fn try_from_json(
        value: JsonTypes,
        param_type: &AbiType,
        arg_name: &str,
    ) -> Result<InputValue, InputParserError> {
        let input_value = match (value, param_type) {
            (JsonTypes::String(string), AbiType::String { .. }) => InputValue::String(string),
            (JsonTypes::String(string), AbiType::Integer { sign: crate::Sign::Signed, width }) => {
                InputValue::Field(parse_str_to_signed(&string, *width, arg_name)?)
            }
            (
                JsonTypes::String(string),
                AbiType::Field | AbiType::Integer { .. } | AbiType::Boolean,
            ) => InputValue::Field(parse_str_to_field(&string, arg_name)?),

            (
                JsonTypes::Integer(integer),
                AbiType::Integer { sign: crate::Sign::Signed, width },
            ) => {
                let new_value = parse_integer_to_signed(integer as i128, *width, arg_name)?;
                InputValue::Field(new_value)
            }

            (
                JsonTypes::Integer(integer),
                AbiType::Field | AbiType::Integer { .. } | AbiType::Boolean,
            ) => {
                let new_value = FieldElement::from(i128::from(integer));

                InputValue::Field(new_value)
            }

            (JsonTypes::Bool(boolean), AbiType::Boolean) => InputValue::Field(boolean.into()),

            (JsonTypes::Array(array), AbiType::Array { typ, .. }) => {
                let mut index = 0;
                let array_elements = try_vecmap(array, |value| {
                    let sub_name = format!("{arg_name}[{index}]");
                    let value = InputValue::try_from_json(value, typ, &sub_name);
                    index += 1;
                    value
                })?;
                InputValue::Vec(array_elements)
            }

            (JsonTypes::Table(table), AbiType::Struct { fields, .. }) => {
                let native_table = try_btree_map(fields, |(field_name, abi_type)| {
                    // Check that json contains a value for each field of the struct.
                    let field_id = format!("{arg_name}.{field_name}");
                    let value = table
                        .get(field_name)
                        .ok_or_else(|| InputParserError::MissingArgument(field_id.clone()))?;
                    InputValue::try_from_json(value.clone(), abi_type, &field_id)
                        .map(|input_value| (field_name.to_string(), input_value))
                })?;

                InputValue::Struct(native_table)
            }

            (JsonTypes::Array(array), AbiType::Tuple { fields }) => {
                let mut index = 0;
                let tuple_fields = try_vecmap(array.into_iter().zip(fields), |(value, typ)| {
                    let sub_name = format!("{arg_name}[{index}]");
                    let value = InputValue::try_from_json(value, typ, &sub_name);
                    index += 1;
                    value
                })?;
                InputValue::Vec(tuple_fields)
            }

            (value, _) => {
                return Err(InputParserError::AbiTypeMismatch(
                    format!("{value:?}"),
                    param_type.clone(),
                ));
            }
        };

        Ok(input_value)
    }
}

#[cfg(test)]
mod test {
    use acvm::FieldElement;
    use proptest::prelude::*;

    use crate::{
        AbiType,
        arbitrary::arb_abi_and_input_map,
        input_parser::{InputValue, arbitrary::arb_signed_integer_type_and_value, json::JsonTypes},
    };

    use super::{parse_json, serialize_to_json};

    proptest! {
        #[test]
        fn serializing_and_parsing_returns_original_input((abi, input_map) in arb_abi_and_input_map()) {
            let json = serialize_to_json(&input_map, &abi).expect("should be serializable");
            let parsed_input_map = parse_json(&json, &abi).expect("should be parsable");

            prop_assert_eq!(parsed_input_map, input_map);
        }

        #[test]
        fn signed_integer_serialization_roundtrip((typ, value) in arb_signed_integer_type_and_value()) {
            let string_input = JsonTypes::String(value.to_string());
            let input_value = InputValue::try_from_json(string_input, &typ, "foo").expect("should be parsable");
            let JsonTypes::String(output_string) = JsonTypes::try_from_input_value(&input_value, &typ).expect("should be serializable") else {
                panic!("wrong type output");
            };
            let output_number = if let Some(output_string) = output_string.strip_prefix("-0x") {
                -i64::from_str_radix(output_string, 16).unwrap()
            } else {
                i64::from_str_radix(output_string.strip_prefix("0x").unwrap(), 16).unwrap()
            };
            prop_assert_eq!(output_number, value);
        }
    }

    #[test]
    fn errors_on_integer_to_signed_integer_overflow() {
        let typ = AbiType::Integer { sign: crate::Sign::Signed, width: 8 };
        let input = JsonTypes::Integer(128);
        assert!(InputValue::try_from_json(input, &typ, "foo").is_err());

        let typ = AbiType::Integer { sign: crate::Sign::Signed, width: 16 };
        let input = JsonTypes::Integer(32768);
        assert!(InputValue::try_from_json(input, &typ, "foo").is_err());
    }

    #[test]
    fn try_from_json_negative_integer() {
        let typ = AbiType::Integer { sign: crate::Sign::Signed, width: 8 };
        let input = JsonTypes::Integer(-1);
        let InputValue::Field(field) = InputValue::try_from_json(input, &typ, "foo").unwrap()
        else {
            panic!("Expected field");
        };
        assert_eq!(field, FieldElement::from(255_u128));
    }
}
