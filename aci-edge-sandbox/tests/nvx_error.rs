// Copyright (c) Microsoft Corporation.
// Licensed under the MIT license.

//! Compatibility of public error conversions with the runtime's public contracts.

#![cfg(feature = "agent")]

use aci_edge_sandboxes::{Error, ErrorCode, nvx};

#[test]
fn converted_nvx_errors_keep_the_io_cause_without_an_extra_wrapper() {
    let directory = tempfile::tempdir().unwrap();
    for classification in [None, Some(ErrorCode::NotStarted)] {
        let original = nvx::host::vm::VmDir::open(&directory.path().join("missing")).unwrap_err();
        assert_eq!(original.kind(), nvx::ErrorKind::Io);
        let message = original.message().to_owned();
        let original_source = std::error::Error::source(&original)
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        let os_error = original_source.raw_os_error();
        assert_eq!(original_source.kind(), std::io::ErrorKind::NotFound);
        let error = match classification {
            Some(code) => Error::from_nvx(code, original),
            None => Error::from(original),
        };
        let code = classification.unwrap_or(ErrorCode::BackendError);
        assert_eq!(error.code(), code);
        assert_eq!(error.message(), message);
        assert_eq!(
            serde_json::to_value(error.body()).unwrap(),
            serde_json::json!({"code": code.wire_code(), "message": message})
        );
        let source = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .unwrap();
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        assert_eq!(source.raw_os_error(), os_error);
    }
}

#[test]
fn refused_nvx_values_keep_the_policy_classification() {
    let original = nvx::host::runtime::ArtifactIdentity::sha256("not-a-digest").unwrap_err();
    assert_eq!(original.kind(), nvx::ErrorKind::Unsupported);
    let message = original.message().to_owned();
    let error = Error::from(original);
    assert_eq!(error.code(), ErrorCode::PolicyValidation);
    assert_eq!(error.message(), message);
    assert_eq!(
        serde_json::to_value(error.body()).unwrap(),
        serde_json::json!({"code": "policy_validation", "message": message})
    );
}
