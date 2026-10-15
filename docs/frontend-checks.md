# Frontend checks

Most frontend rules are checked by `cargo test` without a browser
(`tests/frontend.rs`): no search UI in the page before login, no inline
scripts, styles or event handlers, no `innerHTML`-style APIs in the
scripts, module imports resolve, versioned assets are `immutable`, and the
main page stays under 50 KB.

## Headless Chrome tests

`tests/browser.rs` drives a real Chrome. The tests are ignored by default
because they need one installed:

```sh
# CHROME defaults to the Windows install path, which works from WSL.
CHROME=/usr/bin/google-chrome cargo test --test browser -- --ignored
```

They check:

- The access gate renders under the real CSP with no console errors, CSP
  refusals or uncaught exceptions. A control test makes sure a real CSP
  violation is caught, so the check cannot pass vacuously.
- **Latest query wins**: the page types `du`, and the moment that (slow)
  search is on the wire types `dun`. The `du` answer never renders; only
  `dun`'s results do.
- **Keyboard-only flow**: Arrow Down/Up, Home, End, Enter on a disabled
  (already requested) result does nothing, Enter on an available one opens
  the confirm sheet with focus on **Add to queue**, submitting closes it,
  focus returns to the search box and "My requests" refreshes. Escape
  closes the list, and a second Escape clears the box.
- **Admin page** (`tests/js/admin_driver.js`): six tabs, the Pending row
  with requester, IMDb ID and an `Open in DMM` link (`rel=noopener`), a
  status change that fails rolls back and one that succeeds removes the
  row, Archive history expands, and adding a member shows the code once
  with the "you will not see this again" notice.

The drivers run next to the real page modules and write `PASS` or a list
of failures into the page.

## Manual checks

Run the app locally (`COOKIE_DEV_INSECURE=1`, `PUBLIC_ORIGIN=http://localhost:8000`)
and add a member with `mediaqueue add-member --name Test`.

1. **Real keyboard.** Using only the keyboard: type the code, press Enter,
   type a title, press Arrow Down, Enter, then Enter again on
   **Add to queue**. The status line confirms the request.
2. **Phone.** Code input shows capitals, pasting `abcd efgh` or
   `ABCD-EFGH` gives `ABCD-EFGH`, and the dropdown is usable by touch.
3. **Lockout.** Six wrong codes show a countdown, and the input stays
   disabled until it ends.
4. **Lighthouse.** In Chrome DevTools, open Lighthouse, pick the
   Accessibility category and run it on `/` once logged in. The target is 95
   or higher. From a shell with Node: `npx lighthouse http://localhost:8000/
   --only-categories=accessibility`. This needs a session cookie for the
   logged-in view, so DevTools is easier.
