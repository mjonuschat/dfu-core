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

    assert_eq!(readback, (64..81).collect::<Vec<u8>>());
}

#[test]
fn defers_manifestation_until_after_dfuse_readback() {
    let mock = mock::MockIOBuilder::default()
        .address(64)
        .dfuse(true)
        .build();
    let data = mock.data();
    let firmware = (0..17).collect::<Vec<u8>>();
    let mut dfu = DfuSync::new(mock);
    dfu.override_address(64);

    let deferred = dfu
        .download_without_manifest_from_slice(&firmware)
        .expect("download succeeds without manifestation");

    assert!(!data.manifested());
    assert!(!data.was_reset());
    assert_eq!(data.downloaded(), firmware);

    let (deferred, readback) = deferred
        .upload_from_address(64, 17)
        .expect("readback succeeds before manifestation");
    assert_eq!(readback, (64..81).collect::<Vec<u8>>());
    assert!(!data.manifested());

    let dfu = deferred.manifest().expect("manifestation succeeds");
    assert!(dfu.is_none());
    assert!(data.manifested());
    assert!(data.completed());
    assert!(data.was_reset());
}
