#!/usr/bin/env python3
"""Headless 'browser' for the OAuth e2e — stands in for a human + real browser.

`toonfmt login` invokes its `open_browser` closure with the authorization URL.
In production that shells out to `open`/`xdg-open`; in the e2e we set
`TOONFMT_BROWSER_CMD` to this script. toonfmt appends the auth URL as the final
argv, so we receive it as `sys.argv[1]`.

We just GET that URL and follow redirects. The OAuth stub's `/authorize` endpoint
auto-approves and 302s to toonfmt's loopback `redirect_uri?code=…&state=…`, so the
GET that follows the redirect *is* the callback that unblocks `oauth::accept_callback`.
No consent UI, no human.

toonfmt spawns this and does NOT wait (browser launch is fire-and-forget), so it's
fine for this to run concurrently with the loopback listener accepting the callback.
Errors are swallowed to stderr — the login flow's own timeout is the backstop.
"""

import sys
import urllib.request


def main() -> None:
    if len(sys.argv) < 2:
        print("headless_browser: no URL argument", file=sys.stderr)
        return
    url = sys.argv[-1]
    try:
        # Default opener follows 30x redirects, so the /authorize -> loopback
        # /callback hop happens here, delivering code+state to toonfmt.
        with urllib.request.urlopen(url, timeout=10) as resp:
            resp.read()
    except Exception as e:  # noqa: BLE001 — best-effort; login's timeout is the backstop
        print(f"headless_browser: {e}", file=sys.stderr)


if __name__ == "__main__":
    main()
