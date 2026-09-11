#![allow(dead_code)]

use dfu_core::synchronous::DfuSync;

mod mock;

#[test]
fn reads_back_dfuse_firmware_from_an_overridden_address() {
    let mock = mock::MockIOBuilder::default()
        .address(64)
        .dfuse(true)
        .build();
    let mut dfu = DfuSync::new(mock);
    dfu.override_address(64);

    let (_dfu, readback) = dfu.upload_from_address(64, 17).expect("readback succeeds");

    assert_eq!(readback, (0..17).collect::<Vec<u8>>());
}
