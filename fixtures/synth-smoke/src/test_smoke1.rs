// Copyright Kani Contributors
// SPDX-License-Identifier: Apache-2.0 OR MIT
#[kani::proof]
fn kani_test_1() {
    let value: u8 = kani::any();
    assert_eq!(value ^ value, 0);
}
