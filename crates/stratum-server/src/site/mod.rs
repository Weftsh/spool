//! Static site hosting: what a repository publishes, where it is served
//! from, and how a request for it is told apart from a request for the
//! product.
//!
//! The shape, in one paragraph. A repository with a `.weft/site.yml`
//! publishes a directory; a push to its branch records a **deploy**,
//! which for a site that needs no build is a *pointer* — the commit and
//! the tree of the published directory — and copies no bytes anywhere,
//! because the blobs are already in this repository's store. Serving
//! resolves the request's `Host` to a site, the site to its current
//! deploy, and the path to a blob in that tree.
//!
//! Sites are served from a **different domain** to the product, and that
//! separation is load-bearing rather than cosmetic. It keeps customer
//! pages away from the session cookie, and it makes the dispatch
//! predicate a suffix match on a domain the dashboard never answers on
//! — so the layer that decides "is this a site request" cannot swallow
//! product traffic by accident.

pub mod dispatch;
pub mod host;
pub mod path;
pub mod serve;
pub mod tree;
