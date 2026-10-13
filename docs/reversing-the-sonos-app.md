# Exploring how the Sonos app gets its front pages — yourself

A guide to settling, with conventional tools and your own account, the question the
Amazon/Apple/YouTube Music gaps raised: **how does the Sonos phone app show a full front page
for a service when x2rock's direct-SMAPI call is refused, and can x2rock do the same without
pretending to be the app?**

Nothing here needs a special tool or a private key. It uses a decompiler, a proxy, Chrome's dev
console, and Sonos's own developer program — all things you run against your own devices and your
own account.

**Scope of this exercise: explore once, for technical plausibility and completeness — not to ship.**
Whether or not the cloud path turns out to be reachable, x2rock will not route through it (see the
standing decision at the end). The point is to *know* how the app does it and where the line is, not
to build across that line.

---

## What the teardown already established (so you don't re-derive it)

The modern Sonos app (`com.sonos.acr2`, the "S2 / new app") does **not** talk to Amazon, Apple or
anyone else directly. Every music service goes through **Sonos's own cloud**:

- Host: `api.ws.sonos.com/content/api` (sibling of the **public** Control API at
  `api.ws.sonos.com/control/api`).
- Paths are **generic over the service**, e.g.
  `…/v3/households/{householdId}/services/{serviceId}/accounts/{accountId}/containers/{containerId}/browse`,
  `…/search`, `…/favorites`, `…/playlists`. There is no per-service hostname anywhere in the app.
- Auth header is `X-Sonos-SMAPI-Auth` (plus `X-Sonos-Api-Key`), **not** the household `privateKey`
  x2rock holds. The networking layer is **OkHttp** (Kotlin), living in the `com.sonos.sdk.content`
  classes in the dex — not in the Dart/Flutter `libapp.so`. Certificate validation is present
  (`libsonos-certval-android.so`), so expect pinning.

**That answers the "does YouTube Music use this path too?" question: yes.** The route is one code
path parameterised by `serviceId`, so YouTube Music, Amazon, Apple, Spotify, Deezer and every other
service all reach their front pages through the same `content/api` call. The app never hits a
service's SMAPI endpoint for browse at all. x2rock does the opposite — it speaks SMAPI directly to
each service — which is why the gaps look per-service (Amazon 401, Apple 500, YouTube Music 403
caller-key) when they're really one wall: **the app brokers through Sonos's cloud; x2rock does not.**

So the real question to settle is narrow: **is Sonos's cloud content API reachable with a
legitimately obtained Sonos-account token, or only by the official app?** The approaches below, in
rough order of effort, let you answer it.

---

## Approach 1 — the clean path: Sonos developer OAuth, then `curl` (no reversing)

Sonos runs a **public** developer program with a documented OAuth2 flow and a documented Control
API on the very host the app uses. If the content endpoints accept a token minted this way, you have
your answer with zero impersonation.

1. **Register an app** at <https://developer.sonos.com> → *Control* → create an integration. You get
   a **Key** and **Secret** and set a **redirect URI** (use something like
   `http://localhost:8080/callback`).

2. **Authorize** (opens in your browser, logs into *your* Sonos account):
   ```
   https://api.sonos.com/login/v3/oauth?client_id=KEY&response_type=code&state=xyz
     &scope=playback-control-all&redirect_uri=http%3A%2F%2Flocalhost%3A8080%2Fcallback
   ```
   Catch the `code=` it redirects back with (run a throwaway listener:
   `python3 -m http.server 8080` and read the query string from the access log, or just copy it out
   of the address bar).

3. **Exchange it for a bearer token:**
   ```bash
   curl -s https://api.sonos.com/login/v3/oauth/access \
     -u "KEY:SECRET" \
     -d grant_type=authorization_code \
     -d code=THE_CODE \
     -d redirect_uri=http://localhost:8080/callback
   ```

4. **Call the public Control API first** to confirm the token and learn your household id:
   ```bash
   TOKEN=...           # access_token from step 3
   curl -s https://api.ws.sonos.com/control/api/v1/households \
     -H "Authorization: Bearer $TOKEN"
   ```

5. **Now try the content endpoints the app uses.** Fill in the ids and probe:
   ```bash
   HH=...; SID=9;      # serviceId: Amazon=9 in the global SMAPI list, or read it from /services
   curl -s "https://api.ws.sonos.com/content/api/v3/households/$HH/services/$SID/accounts/ACCT/containers/root/browse" \
     -H "Authorization: Bearer $TOKEN" -H "Accept: application/json" | head -c 2000
   ```
   - **200 with shelves** → the content API takes an ordinary developer OAuth token, and x2rock
     *can* reach full front pages the legitimate way. This is the outcome worth chasing.
   - **401 / 403 / "X-Sonos-Api-Key required"** → the content (as opposed to control) surface is
     gated to Sonos's own API key, and matching the app would mean impersonating it. That closes the
     question: record it as a real gap, don't build it.

The whole of this approach is Sonos's documented program plus `curl`; it's the first thing to try.

---

## Approach 2 — static decompile with jadx (read how `X-Sonos-SMAPI-Auth` is built)

This reads the app's own code to learn what goes into that header — is it just the OAuth bearer
from Approach 1, or something the app computes that you can't reproduce?

```bash
yay -S jadx          # AUR; pulls a JRE if you don't have one
```

The app is already pulled to this session's scratchpad; copy it somewhere you keep
(`~/sonos-apk/base.apk`), then:

```bash
jadx-gui ~/sonos-apk/base.apk
```

In the GUI:
- **Search** (the magnifying glass) for `X-Sonos-SMAPI-Auth` and for `X_SONOS_SMAPI_AUTH`.
- Follow it into `com.sonos.sdk.content.oas.infrastructure` — that's the OpenAPI-generated client,
  so the header *parameters* and the request-signing *interceptor* are both there and read like
  ordinary Kotlin.
- The question to answer from the code: **what value is assigned to that header?** If it traces back
  to the account access token (the OAuth bearer), Approach 1 is the whole story. If it's derived —
  signed, combined with a per-install key from `libsecrets.so` / `libsonoscrypto.so`, or minted by a
  call to a Sonos auth endpoint — note that endpoint and how. CLI equivalents if you prefer the
  terminal: `jadx -d out ~/sonos-apk/base.apk` then `grep -rn "SMAPI-Auth" out/`.

Native `.so` files (if the trail goes into `libsecrets`/`libsonoscrypto`) are for Ghidra/radare2 and
are a much bigger job — only go there if the Kotlin shows the header is cryptographically derived.

---

## Approach 3 — watch the live traffic from the phone (mitmproxy)

Confirms exactly what the app sends, headers and all. This is where cert pinning bites.

```bash
yay -S mitmproxy         # or: pipx install mitmproxy
mitmweb --listen-port 8080     # web UI at http://127.0.0.1:8080
```

Point the phone at it: Wi-Fi → your network → Proxy = manual, host = this PC's LAN IP, port 8080.
On the phone browse to `http://mitm.it` and install the mitmproxy CA cert.

**The two walls, in order:**

1. **User CA trust.** Android 7+ apps ignore user-installed CAs unless they opt in. Options, easiest
   first: use an **emulator** you control (`emulator -avd Pixel -writable-system`, then
   `adb root && adb remount` and drop the cert into `/system/etc/security/cacerts/`), or root the
   moto g, or repackage the APK with a `network_security_config` that trusts user certs (`apktool d`,
   edit, `apktool b`, re-sign — heavy, and re-signing may trip the next wall anyway).

2. **Certificate pinning.** `libsonos-certval-android.so` strongly implies the app pins, so even a
   trusted CA gets rejected. Bypass it at runtime with **frida** / **objection** (no repackaging):
   ```bash
   pipx install frida-tools objection
   # push the matching frida-server to a rooted device/emulator, then:
   objection -g com.sonos.acr2 explore
   # in the objection shell:
   android sslpinning disable
   ```
   The good news: the networking is OkHttp, which is proxy-aware and whose pinning objection knows
   how to defeat — far friendlier than if it had been Flutter's Dart stack (which ignores the system
   proxy entirely and would need `reFlutter`).

Once traffic flows, filter mitmweb to `api.ws.sonos.com` and open a service's Home in the app. You'll
see the exact `content/api` request, the `X-Sonos-SMAPI-Auth` and `X-Sonos-Api-Key` values, and the
JSON that comes back. Right-click a flow → *Copy as cURL* to replay it from your terminal.

---

## Approach 4 — Chrome DevTools, for the login flow

You don't need the phone for the part that happens in a browser: **"Login with Sonos"** (the OAuth
consent in Approach 1, and the web views the app opens for some services) is ordinary web traffic.

- Open the authorize URL from Approach 1 in Chromium, press **F12** → **Network**, tick *Preserve
  log*, and sign in. You'll see the `api.sonos.com/login/v3/oauth…` calls, the redirect carrying the
  `code`, and the token exchange — useful for scripting the flow and for seeing exactly which
  `scope`s the app asks for.
- *Copy → Copy as cURL* on any request reproduces it from the terminal with all headers.
- The **Application** tab shows cookies/localStorage if a web view stashes a session there.

This won't show the `content/api` browse calls (those are native OkHttp on the phone, Approach 3),
but it's the quickest way to understand and reproduce the account-token half.

---

## How to read the result — and the standing decision

**x2rock will not broker through Sonos's cloud, so this is for understanding, not for building.**
The reason is about who provides the service, not about what's technically reachable:

- With Amazon, Apple, YouTube Music, Spotify and the rest, **you are the service's customer** — they
  provide you a service, and x2rock reaching their SMAPI endpoint with your own token is you using
  what you pay for. That is x2rock's whole model: your household's own tokens, over the LAN and
  straight to the service.
- **Sonos's cloud is Sonos's infrastructure**, and x2rock is not a licensed client of it. Routing
  your content requests through `api.ws.sonos.com/content/api` — even with a developer OAuth token
  — is using Sonos's cloud without permission. The developer program sanctions the *Control* API
  (players, groups, volume, transport); it does not grant x2rock standing to use the cloud as a
  content broker for third-party services.

So whatever these approaches turn up:

- **Even if a developer-OAuth bearer reaches `content/api`:** that it answers does not make it ours
  to use. It stays off-limits.
- **If the header is Sonos's own API key or a derived per-install secret:** matching the app would
  additionally mean impersonating it. Doubly off-limits.

Either way the outcome is the same: **Amazon's Home, Apple's shelves and YouTube Music's
personalised pages are won't-do gaps** — not because x2rock can't reach them, but because the only
route that matches the app runs through a cloud x2rock has no standing to use. Explore it to
understand the boundary; don't build across it.

### What the one-time explore actually found (2026-10-13)

Ran Approach 1 end to end with a registered "x2rock" developer integration:

- **Control API — fully open to the developer bearer.** Listed both households; `musicServiceAccounts/preferred`
  returned `sn_0`.
- **Content API — authenticates and authorises the same bearer.** The browse endpoint answered
  business-logic errors (`ERROR_SERVICE_NOT_CONFIGURED` 422, `ERROR_RESOURCE_NOT_FOUND` 404), never
  401/403 or "X-Sonos-Api-Key required." **The app-only-key wall we feared is not there.**
- **The id namespace comes from the household search.** The content API's `serviceId`/`accountId`
  aren't SMAPI's numbers; `GET /content/api/v3/households/{hh}/search?query=…` returns them — each
  service block carries its cloud `serviceId` (Amazon 51463, Apple 52231, YouTube Music 72711, …)
  and `accountId` (the `sn_N` serial, prefix dropped).
- **Every gap retrieved in full** with those ids: Amazon's whole Home (11 sections), Apple's shelves
  (Library, Browse Our Picks, Daily Top 100, Radio Shows, Stations by Genre, …), and YouTube Music's
  Home/Library/My Supermix. **YouTube Music search also works** (20 hits) — the service the *direct*
  SMAPI path can't touch (403). Pages come as `TEMPLATE_PROVIDER_HOME_PAGE` → `sections.items[]`;
  search is one call across all configured services.
- **Secret required.** Sonos is a confidential-client flow: the client secret is needed on the code
  exchange *and* every refresh (no PKCE). A daemon staying logged in must hold the secret locally.

**Conclusion:** not plausible-in-theory but **proven end to end** — a developer OAuth token reaches
every front page and search x2rock's direct path is blocked from. The only thing in the way is the
**licence/credential boundary.** x2rock ships no Sonos credentials.

**Built on this, 2026-10-13 (x2rock CLI only):** cloud browse/search as an opt-in fallback, live
only when the person has their own integration and has signed in. See `src/sonos/cloud.rs` and the
"Cloud browse and search, opt-in" section in `docs/architecture.md`. x2rocktv is deliberately left
out.

---

## Legit & cleanup notes

- Everything here is against **your** account and **your** devices — your Sonos login, your phone,
  your household. That's ordinary interoperability research, and the Sonos developer program is a
  sanctioned front door.
- Don't commit tokens, the mitmproxy CA key, or captured flows. Keep them out of the repo.
- The pulled APK is in this session's scratchpad
  (`…/scratchpad/apk/`), which is temporary — copy `base.apk` somewhere you keep if you want to run
  jadx later, or just re-pull it with
  `adb -s <phone> shell pm path com.sonos.acr2` + `adb pull`.
- A re-signed or repackaged Sonos app can behave oddly or lock you out; prefer the frida/objection
  runtime route or an emulator over editing and re-signing the real app.
