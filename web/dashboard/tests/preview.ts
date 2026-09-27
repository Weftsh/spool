/// Where the preview server this suite drives is listening.
///
/// One constant, read by `playwright.config.ts` and by the handful of
/// specs that need the origin as a *value* — a `detail_url` on this
/// origin, the register command the runners page prints. Those specs
/// used to spell `http://127.0.0.1:4173` themselves, with a comment
/// saying "this origin is the preview server playwright.config.ts points
/// at", which was true and unenforced: the port lived in two places and
/// the tests that would break if they disagreed are the ones about
/// origin matching, where a mismatch reads as the feature being broken.
///
/// **The port is overridable because this machine runs several
/// worktrees.** `--strictPort` plus `CI=true` (which `ci-local.sh` sets
/// on purpose, so the local run reproduces CI rather than approximating
/// it) means two checkouts running the web gate at once collide — and
/// the collision does not present as a port error. It presents as a run
/// where every test passes and the process exits non-zero, which reads
/// as a product failure and cost an afternoon. A peer session on this
/// machine had resorted to a hand-written `lsof` wait loop.
///
/// `scripts/test-env.sh` picks a free port per worktree and exports it;
/// unset, this is the 4173 it always was, so nothing about a single
/// checkout or about CI changes.
export const PREVIEW_PORT = Number(process.env.STRATUM_PREVIEW_PORT ?? 4173);

export const PREVIEW_ORIGIN = `http://127.0.0.1:${PREVIEW_PORT}`;
