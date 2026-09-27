//! Deciding whether a request is for a customer's site or for the
//! product, from its `Host` alone.
//!
//! This is the highest-consequence function in the feature, and the
//! consequence is not a 404. The router ends in a fallback that serves
//! the marketing site and, failing that, the dashboard's own single-page
//! app (`webassets::site`). So a site request that fell through to the
//! router would answer a customer's domain with **our product's UI**,
//! and a product request wrongly claimed as a site would take the
//! dashboard off the air.
//!
//! Two things keep that safe, and both are structural rather than
//! careful coding.
//!
//! Sites live on a **different registered domain** to the product, so the
//! predicate is a suffix match against a domain the dashboard never
//! answers on. There is no overlap to get wrong, no path prefix to
//! reserve, and no cookie shared between the two.
//!
//! And the decision **fails toward the product**. Every uncertain case —
//! no sites domain configured, a `Host` we cannot parse, a label with a
//! dot in it — is [`Dispatch::Product`], which is the behaviour the
//! server has today. A bug here can fail to serve a site; it cannot take
//! the product down.

/// What a request's `Host` says it is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// Not the sites domain. The ordinary router answers it, exactly as
    /// it does today.
    Product,
    /// The apex of the sites domain itself. Never a customer's site, and
    /// never the dashboard either — putting product UI here would undo
    /// the cookie isolation the separate domain exists for.
    Apex,
    /// A site, by the DNS label it is served at.
    Site(String),
}

/// Strip the port and lowercase, the two ways a `Host` differs from what
/// is stored without meaning anything different.
///
/// IPv6 literals are bracketed (`[::1]:8080`) and are never a site host,
/// but they must not be mangled into one by a naive split on `:`.
fn normalise(host: &str) -> Option<String> {
    let h = host.trim();
    if h.is_empty() {
        return None;
    }
    let without_port = if let Some(rest) = h.strip_prefix('[') {
        // `[::1]:8080` — everything to the closing bracket.
        let end = rest.find(']')?;
        return Some(format!("[{}]", &rest[..end]).to_ascii_lowercase());
    } else {
        match h.split_once(':') {
            Some((a, _)) => a,
            None => h,
        }
    };
    if without_port.is_empty() {
        return None;
    }
    // A trailing dot is a fully-qualified name and means the same thing.
    Some(without_port.trim_end_matches('.').to_ascii_lowercase())
}

/// Is this a sites domain we are willing to serve on?
///
/// Refuses a bare TLD or an empty string, because a suffix match against
/// `"sh"` would claim `weft.sh` — the product — as a site request. The
/// boot check calls this so a misconfiguration is a startup failure
/// rather than a dashboard that has quietly become a 404.
pub fn is_plausible_domain(domain: &str) -> bool {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    d.len() >= 4
        && d.contains('.')
        && !d.starts_with('.')
        && !d.ends_with('.')
        && d.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// Classify a request.
///
/// `sites_domain` is `None` on a deployment with site hosting switched
/// off, and then everything is [`Dispatch::Product`] — the behaviour
/// before this feature existed.
pub fn classify(host_header: Option<&str>, sites_domain: Option<&str>) -> Dispatch {
    let Some(domain) = sites_domain else {
        return Dispatch::Product;
    };
    if !is_plausible_domain(domain) {
        // Should be unreachable past the boot check. Failing toward the
        // product rather than trusting it is the whole disposition of
        // this module.
        return Dispatch::Product;
    }
    let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    let Some(host) = host_header.and_then(normalise) else {
        return Dispatch::Product;
    };
    if host == domain {
        return Dispatch::Apex;
    }
    let Some(label) = host.strip_suffix(&format!(".{domain}")) else {
        return Dispatch::Product;
    };
    // Exactly one label. `a.b.sites.example` is not a site: the wildcard
    // certificate covers one level, so we could not serve it over HTTPS
    // even if we wanted to, and answering it over plain HTTP is not an
    // option for customer content.
    if label.is_empty() || label.contains('.') {
        return Dispatch::Product;
    }
    if !super::host::is_valid_label(label) {
        return Dispatch::Product;
    }
    Dispatch::Site(label.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Option<&str> = Some("weft.example");

    #[test]
    fn with_no_sites_domain_everything_is_the_product() {
        assert_eq!(
            classify(Some("docs--acme.weft.example"), None),
            Dispatch::Product
        );
        assert_eq!(classify(Some("weft.sh"), None), Dispatch::Product);
    }

    #[test]
    fn a_label_under_the_sites_domain_is_a_site() {
        assert_eq!(
            classify(Some("docs--acme.weft.example"), D),
            Dispatch::Site("docs--acme".into())
        );
    }

    #[test]
    fn the_apex_is_its_own_answer_and_never_a_site() {
        assert_eq!(classify(Some("weft.example"), D), Dispatch::Apex);
    }

    /// The product must keep working. These are the hosts the dashboard,
    /// the API and the git wire actually arrive on.
    #[test]
    fn the_products_own_hosts_are_never_claimed() {
        for h in [
            "weft.sh",
            "www.weft.sh",
            "api.weft.sh",
            "127.0.0.1",
            "localhost",
            "stratum-prod-1742123756.us-east-1.elb.amazonaws.com",
        ] {
            assert_eq!(classify(Some(h), D), Dispatch::Product, "{h}");
        }
    }

    /// A suffix match is only safe if it is a match on `.domain`, not on
    /// `domain`. `notweft.example` must not be read as the label `not`.
    #[test]
    fn a_domain_that_merely_ends_with_ours_is_not_ours() {
        assert_eq!(classify(Some("notweft.example"), D), Dispatch::Product);
        assert_eq!(classify(Some("evilweft.example"), D), Dispatch::Product);
    }

    #[test]
    fn a_port_and_a_trailing_dot_and_case_all_mean_the_same_host() {
        for h in [
            "docs--acme.weft.example:8080",
            "DOCS--ACME.WEFT.EXAMPLE",
            "docs--acme.weft.example.",
            "  docs--acme.weft.example  ",
        ] {
            assert_eq!(
                classify(Some(h), D),
                Dispatch::Site("docs--acme".into()),
                "{h}"
            );
        }
    }

    /// The wildcard certificate covers one label, so a deeper name could
    /// not be served over HTTPS and must not be served at all.
    #[test]
    fn a_deeper_name_is_not_a_site() {
        assert_eq!(classify(Some("a.b.weft.example"), D), Dispatch::Product);
        assert_eq!(classify(Some(".weft.example"), D), Dispatch::Product);
    }

    #[test]
    fn a_label_dns_would_not_carry_is_not_a_site() {
        for h in [
            "under_score.weft.example",
            "-lead.weft.example",
            "trail-.weft.example",
        ] {
            assert_eq!(classify(Some(h), D), Dispatch::Product, "{h}");
        }
    }

    #[test]
    fn a_missing_or_unparseable_host_is_the_product() {
        assert_eq!(classify(None, D), Dispatch::Product);
        assert_eq!(classify(Some(""), D), Dispatch::Product);
        assert_eq!(classify(Some("   "), D), Dispatch::Product);
        assert_eq!(classify(Some(":8080"), D), Dispatch::Product);
    }

    #[test]
    fn an_ipv6_literal_is_the_product_and_is_not_mangled_on_the_way() {
        assert_eq!(classify(Some("[::1]:8080"), D), Dispatch::Product);
        assert_eq!(classify(Some("[fe80::1]"), D), Dispatch::Product);
    }

    /// The configuration footgun this guards: a sites domain of `sh`
    /// would make a suffix match claim `weft.sh`, the product.
    #[test]
    fn a_sites_domain_that_would_swallow_the_product_is_not_plausible() {
        for bad in ["", " ", "sh", "com", ".sh", "weft.", "a.b c", "x"] {
            assert!(!is_plausible_domain(bad), "{bad:?} should be implausible");
        }
        for good in ["weft.so", "weft.example", "sites.weft.sh"] {
            assert!(is_plausible_domain(good), "{good:?} should be plausible");
        }
    }

    /// …and if one somehow reached `classify`, it still fails toward the
    /// product rather than taking the dashboard off the air.
    #[test]
    fn an_implausible_domain_fails_toward_the_product() {
        assert_eq!(classify(Some("weft.sh"), Some("sh")), Dispatch::Product);
        assert_eq!(classify(Some("anything"), Some("")), Dispatch::Product);
    }
}
