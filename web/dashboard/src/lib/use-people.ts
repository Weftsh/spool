import { useEffect, useState } from "react";
import { api, type Session } from "@/api";

/// user_id → email, so an administrator auditing credentials sees whose
/// they are. Only an admin can read the roster, so this quietly stays
/// empty for everyone else.
export function usePeople(
  session: Session,
  enabled: boolean,
): Record<string, string> {
  const [people, setPeople] = useState<Record<string, string>>({});
  useEffect(() => {
    if (!enabled) return;
    let alive = true;
    api
      .members(session)
      .then((ms) => {
        if (!alive) return;
        setPeople(Object.fromEntries(ms.map((m) => [m.user_id, m.email])));
      })
      .catch(() => undefined);
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.org, session.token, enabled]);
  return people;
}
