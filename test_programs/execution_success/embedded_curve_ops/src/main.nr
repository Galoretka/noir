use std::{embedded_curve_ops::embedded_curve_add_unsafe, ops::Add};

fn main(priv_key: Field, pub_x: pub Field, pub_y: pub Field) {
    let g1 = std::embedded_curve_ops::EmbeddedCurvePoint::generator();
    let scalar = std::embedded_curve_ops::EmbeddedCurveScalar { lo: priv_key, hi: 0 };
    // Test that multi_scalar_mul correctly derives the public key
    let res = std::embedded_curve_ops::multi_scalar_mul([g1], [scalar]);
    assert(res.x == pub_x);
    assert(res.y == pub_y);

    // Test that double function calling embedded_curve_add works as expected
    let pub_point =
        std::embedded_curve_ops::EmbeddedCurvePoint { x: pub_x, y: pub_y, is_infinite: false };
    let res = pub_point.double();
    let double = g1.add(g1);

    assert(double.x == res.x);

    // Test calling multi_scalar_mul with multiple points and scalars
    let res = std::embedded_curve_ops::multi_scalar_mul([g1, g1], [scalar, scalar]);

    // The results should be double the g1 point because the scalars are 1 and we pass in g1 twice
    assert(double.x == res.x);

    // Tests for #6549
    let const_scalar1 = std::embedded_curve_ops::EmbeddedCurveScalar { lo: 23, hi: 0 };
    let const_scalar2 = std::embedded_curve_ops::EmbeddedCurveScalar { lo: 0, hi: 23 };
    let const_scalar3 = std::embedded_curve_ops::EmbeddedCurveScalar { lo: 13, hi: 4 };
    let partial_mul = std::embedded_curve_ops::multi_scalar_mul(
        [g1, double, pub_point, g1, g1],
        [scalar, const_scalar1, scalar, const_scalar2, const_scalar3],
    );
    assert(partial_mul.x == 0x2024c4eebfbc8a20018f8c95c7aab77c6f34f10cf785a6f04e97452d8708fda7);
    // Check simplification by zero
    let zero_point = std::embedded_curve_ops::EmbeddedCurvePoint { x: 0, y: 0, is_infinite: true };
    let const_zero = std::embedded_curve_ops::EmbeddedCurveScalar { lo: 0, hi: 0 };
    let partial_mul = std::embedded_curve_ops::multi_scalar_mul(
        [zero_point, double, g1],
        [scalar, const_zero, scalar],
    );
    assert(partial_mul == g1);

    // Additional tests for validating embedded_curve_add_unsafe under a conditional
    if pub_x == pub_y {
        let a1 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 1, y: 2, is_infinite: false };
        let a2 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 1, y: 3, is_infinite: false };
        let doubling = a1.double();
        assert(doubling.x == 1);
        let res = embedded_curve_add_unsafe(a1, a2);
        assert(res.x == 1);

        let a1 = std::embedded_curve_ops::EmbeddedCurvePoint {
            x: pub_x + 1,
            y: pub_y,
            is_infinite: false,
        };
        let a2 = std::embedded_curve_ops::EmbeddedCurvePoint {
            x: pub_x + 1,
            y: pub_y + 1,
            is_infinite: false,
        };
        let doubling = a1.double();
        assert(doubling.x == 1);
        let res = embedded_curve_add_unsafe(a1, a2);
        assert(res.x == 1);

        let a1 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 2, y: 3, is_infinite: false };
        let a2 = std::embedded_curve_ops::EmbeddedCurvePoint {
            x: pub_x,
            y: pub_y + 1 as Field,
            is_infinite: false,
        };
        let res = embedded_curve_add_unsafe(a1, a2);
        assert(res.x == 1);

        let a1 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 2, y: 3, is_infinite: false };
        let a2 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 2, y: 4, is_infinite: false };
        let res = embedded_curve_add_unsafe(a1, a2);
        assert(res.x == 1);

        let a1 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 2, y: 3, is_infinite: false };
        let a2 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 2, y: 3, is_infinite: false };
        let res = embedded_curve_add_unsafe(a1, a2);
        assert(res.x == 1);
        let a1 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 1, y: 3, is_infinite: false };
        let a2 = std::embedded_curve_ops::EmbeddedCurvePoint { x: 1, y: 3, is_infinite: false };
        let res = embedded_curve_add_unsafe(a1, a2);
        assert(res.x == 1);
        let a1 = std::embedded_curve_ops::EmbeddedCurvePoint { x: pub_x, y: 3, is_infinite: false };
        let a2 = std::embedded_curve_ops::EmbeddedCurvePoint { x: pub_x, y: 2, is_infinite: false };
        let res = embedded_curve_add_unsafe(a1, a2);
        assert(res.x == 1);
    }
}
