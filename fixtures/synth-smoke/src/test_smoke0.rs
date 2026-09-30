// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT
#[kani::proof]
fn kani_test_0() {
    let value: u8 = kani::any();
    assert_eq!(value.wrapping_add(1).wrapping_sub(1), value);
}
