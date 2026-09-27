//! Run the store contract against a live backend and report what it did.
//!
//! This is the manual half of the gate. `crates/stratum-testkit/tests/
//! store_contract.rs` runs the same cases against MinIO on every CI run;
//! this binary points them at a real bucket, which CI cannot do because
//! credentials do not belong there.
//!
//! It is a binary rather than a `#[test]` deliberately: the manual run
//! wants no test harness, no MinIO download, and no dev-dependencies —
//! just credentials, a bucket, and an exit code.
//!
//!     STRATUM_STORE_URL=https://s3.us-east-1.amazonaws.com/my-bucket \
//!     AWS_ACCESS_KEY_ID=… AWS_SECRET_ACCESS_KEY=… AWS_REGION=us-east-1 \
//!       cargo run -p stratum-store --bin store-contract -- --prefix contract-1234
//!
//! Two things about *how* to run it, both of which decide whether the
//! result means anything:
//!
//!   * Run it against **both** addressing styles. `ObjectStore::new`
//!     derives its signing path prefix from the URL's shape, so
//!     `https://s3.<region>.amazonaws.com/<bucket>` and
//!     `https://<bucket>.s3.<region>.amazonaws.com` exercise different
//!     code and can disagree.
//!   * Run it with the **deployment's** IAM policy, not an admin key.
//!     `a_missing_key_reports_404_not_403` exists to catch a
//!     least-privilege policy turning absence into AccessDenied, and an
//!     admin key can never fail it.

use std::process::ExitCode;

use stratum_store::{LatencyModel, ObjectStore};
use stratum_testkit::contract;

fn main() -> ExitCode {
    let mut prefix = String::from("store-contract");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--prefix" => match args.next() {
                Some(p) => prefix = p,
                None => {
                    eprintln!("--prefix needs a value");
                    return ExitCode::FAILURE;
                }
            },
            "-h" | "--help" => {
                eprintln!(
                    "usage: store-contract [--prefix KEY_PREFIX]\n\n\
                     Reads STRATUM_STORE_URL and the AWS_* credentials from the \
                     environment.\nEvery key it writes lives under KEY_PREFIX and is \
                     deleted as it goes."
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unknown argument {other:?}");
                return ExitCode::FAILURE;
            }
        }
    }

    let url = match std::env::var("STRATUM_STORE_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!(
                "STRATUM_STORE_URL is not set. This tool writes to a real bucket; \
                 it will not guess which one."
            );
            return ExitCode::FAILURE;
        }
    };
    if std::env::var("AWS_ACCESS_KEY_ID").is_err() {
        eprintln!(
            "warning: no AWS_ACCESS_KEY_ID — requests will go out unsigned. \
             Against real S3 that fails every case for the wrong reason."
        );
    }

    let store = ObjectStore::new(&url, LatencyModel::None);
    println!("store-contract");
    println!("  endpoint: {url}");
    println!("  prefix:   {prefix}");
    println!();

    let report = contract::run_all(&store, &prefix);

    for case in contract::cases() {
        match report.failed.iter().find(|(n, _)| *n == case.name) {
            None => println!("  PASS  {}", case.name),
            Some((_, why)) => {
                println!("  FAIL  {}", case.name);
                println!("        {why}");
                println!("        relied on by: {}", case.relied_on_by);
            }
        }
    }

    if !report.observed.is_empty() {
        println!("\nobserved (recorded, not asserted):");
        for (what, value) in &report.observed {
            println!("  {what}: {value}");
        }
    }

    println!(
        "\n{} passed, {} failed, of {} cases",
        report.passed.len(),
        report.failed.len(),
        contract::cases().len()
    );

    if report.ok() {
        println!(
            "\nThe contract holds here. Record the endpoint and addressing style \
             with this result — a pass against one addressing style is not a pass \
             against the other."
        );
        ExitCode::SUCCESS
    } else {
        println!(
            "\nThe contract does NOT hold on this backend. Every failure above names \
             the code that depends on the semantic, which is where the breakage will \
             surface in production."
        );
        ExitCode::FAILURE
    }
}
