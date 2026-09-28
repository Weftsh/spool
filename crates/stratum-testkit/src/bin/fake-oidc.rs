//! The manual stack's stand-in identity provider.
//!
//! The same fake the SSO suite signs people in through
//! ([`stratum_testkit::oidc`]), run as a process so that
//! `scripts/manual-stack.sh` can point a real server at it and the
//! walkthrough can sign in the way a person does: click **Continue with**,
//! land on the provider's page, type an address, and come back signed in.
//! Nobody is configured to sign in, so every visit to `/authorize` serves
//! that page.
//!
//!     FAKE_OIDC_ADDR=127.0.0.1:29130 FAKE_OIDC_CLIENT_ID=… \
//!       FAKE_OIDC_CLIENT_SECRET=… fake-oidc
//!
//! It is a stand-in, not the contract: what a real provider sends is
//! checked by `scripts/manual-oidc.sh`, against a real one.

fn main() {
    let var = |k: &str| {
        std::env::var(k).unwrap_or_else(|_| {
            eprintln!("fake-oidc: {k} is required");
            std::process::exit(2);
        })
    };
    let addr = std::env::var("FAKE_OIDC_ADDR").unwrap_or_else(|_| "127.0.0.1:29130".into());
    let fake = stratum_testkit::oidc::spawn_on(
        &addr,
        &var("FAKE_OIDC_CLIENT_ID"),
        &var("FAKE_OIDC_CLIENT_SECRET"),
    );
    println!("fake-oidc: issuer {}", fake.issuer);
    loop {
        std::thread::park();
    }
}
