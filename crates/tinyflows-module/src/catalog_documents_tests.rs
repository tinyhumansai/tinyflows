use super::*;

#[test]
fn shared_catalog_records_are_key_fenced_and_never_scanned() {
    assert!(
        Documents::validate_record("tsd.migrations", "flows/flows.definitions/definitions_v2")
            .is_ok()
    );
    assert!(Documents::validate_record("tsd.migrations", "cron/cron.jobs/jobs_v2").is_err());
    assert!(Documents::validate_record("tsd.migrations", "flows/other/step").is_err());
    assert!(Documents::validate_record("tsd.kv", "flows/kv:state/key").is_ok());
    assert!(Documents::validate_record("tsd.kv", "cron/state/key").is_err());
    assert!(Documents::validate_record("tsd.leases", "cron/job/occurrence").is_err());
    assert!(Documents::validate_set("tsd.migrations").is_err());
    assert!(Documents::validate_set("tsd.kv").is_err());
    assert!(Documents::validate_set("tsd.leases").is_err());
    assert!(Documents::validate_collection("flows_revisions").is_ok());
    assert!(Documents::validate_collection("unrelated").is_err());
}
