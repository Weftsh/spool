import { useEffect, useState } from "react";
import { api, type Billing, type Session } from "@/api";
import { Paywall } from "@/components/paywall";

/// The organization's pools, read for a private repository and for
/// nothing else.
///
/// A public repository is never refused at a spend limit, and `null`
/// visibility — the row still in flight — is not "private" either: a
/// wall drawn on a guess and then withdrawn is a page that flickers a
/// refusal at somebody. Refusals are swallowed the way `SeatLine` does:
/// a member who is not an admin gets 403 from billing, a stranger on
/// the public forge gets a masked 404, and in both cases there is
/// nothing to say to them here.
export function useRepoBilling(
  session: Session,
  isPrivate: boolean | null,
): Billing | null {
  const [billing, setBilling] = useState<Billing | null>(null);
  const { org, token } = session;
  useEffect(() => {
    if (isPrivate !== true) {
      setBilling(null);
      return;
    }
    let alive = true;
    api
      .billing({ org, token })
      .then((b) => alive && setBilling(b))
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [isPrivate, org, token]);
  return billing;
}

/// The spend-limit walls over a private repository, wherever a person
/// lands on one.
///
/// They were on the metrics page alone, which was reached only by
/// clicking a row in the organization's list — so somebody opening a
/// private repository at the cap from a link saw the tree and nothing
/// else, and found the refusal at `git clone`. Now the same component
/// sits over every surface that opens a repository, and each answers
/// the one question it needs — is this repository private — either from
/// the row it already has or, for the file browser that has none, from
/// the row itself.
///
/// Only on the server's flags. Nothing here reads a sentence or does
/// arithmetic: a pool past its edge is metered, not refused, until the
/// spend limit says otherwise, and only the server knows that.
export function RepoWalls(props: {
  session: Session;
  repo: string;
  /// `true`/`false` from a row the caller holds; `null` while the
  /// caller's row is in flight; absent when the caller has no row at
  /// all, in which case it is read here.
  isPrivate?: boolean | null;
}) {
  const given = props.isPrivate;
  const [read, setRead] = useState<boolean | null>(null);
  const { org, token } = props.session;
  useEffect(() => {
    if (given !== undefined) return;
    let alive = true;
    setRead(null);
    api
      .repo({ org, token }, props.repo)
      .then((r) => alive && setRead(!r.public))
      // Unknown is not private: no row, no wall.
      .catch(() => undefined);
    return () => {
      alive = false;
    };
  }, [given, org, token, props.repo]);
  const isPrivate = given === undefined ? read : given;
  const billing = useRepoBilling(props.session, isPrivate);
  if (isPrivate !== true || billing === null) return null;
  const transferRefused = billing.meters?.egress_gb.refusing === true;
  const storageRefused = billing.meters?.storage_gb.refusing === true;
  if (!transferRefused && !storageRefused) return null;
  return (
    <>
      {transferRefused && (
        <Paywall session={props.session} reason="transfer" billing={billing} />
      )}
      {storageRefused && (
        <Paywall session={props.session} reason="storage" billing={billing} />
      )}
    </>
  );
}
