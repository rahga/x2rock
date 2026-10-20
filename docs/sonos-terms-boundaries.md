# Technically possible, and what the terms allow

This note records, for anyone extending x2rock, the places where something is technically
possible but sits at or past the edge of the
[Sonos Platform Terms of Service](https://developer.sonos.com/terms-of-service). It records
conclusions, not how they were reached.

It is a maintainer's reading of those terms (as read 2026-10-14), not legal advice.

## Who the terms bind

The terms bind whoever registers a Sonos developer account. The maintainer has one; so does
anyone who follows [Households with Authentication on](../README.md#households-with-authentication-on).
Ordinary LAN control needs no account. The terms neither license nor forbid it for someone who
never registered.

The clauses that matter here:

- **§2(a)** licenses the Cloud APIs for developing and distributing an app.
- **§2(b)** licenses the LAN APIs "solely for internal evaluation and testing"; publishing or
  distributing a LAN-based app needs a separate licence from Sonos.
- **§3(a)** forbids collecting or recording content data obtained through developer access.
- **§3(g)** forbids reverse engineering Sonos software, including the APIs.
- **§7(b)** treats non-public portal material (API syntax, methods, documentation) as confidential.

## Rules for future work

- Do not decompile, intercept, repackage or otherwise reverse engineer Sonos apps or firmware.
  If a question can only be answered that way, record it as an open gap.
- Prefer, in order: the documented Control API; behaviour any LAN controller can observe from
  the speakers; publicly published third-party interoperability work, cited.
- Never cache content obtained through a developer token (§3(a)).
- Ship no Sonos key or secret, ever. A person who wants an account-based feature registers their
  own integration and accepts the terms themselves.

## The gates

| Gate | Technically | Against the terms | Status |
|---|---|---|---|
| **Search and browse for Amazon Music, Apple Music and YouTube Music** as the Sonos app shows them | Sonos's cloud content service serves them to a Sonos developer sign-in: Amazon's Home, Apple's shelves, YouTube Music browse and search (verified 2026-10-13, office household). Nothing about the implementation is unusual | **Not usable under the current terms.** The terms of the Sonos developer program restrict the content service to invite-only licensees | **Inactive under current license requirements** (2026-10-14): [the cloud path](../README.md#optional-sonos-cloud-search-and-browse) stays in the code with `cloud::ACTIVE` false, so it makes no request |
| **The household's music-service tokens** are stored on the speakers, encrypted to the household | Any LAN controller can read them: the player publishes the encrypted set in its `ZoneGroupTopology` event, and the key is derived from the household id and a fixed salt that originates in Sonos's own software (see below). Method published by a third party ([SoCo PR #1010](https://github.com/SoCo/SoCo/pull/1010)); verified 2026-09-22 | x2rock derived nothing from Sonos materials, but the salt it uses did originate there. Whether §3(g) reaches *using* a salt someone else extracted, versus extracting it, is untested — and a separate, non-ToS question of circumvention law (e.g. US DMCA §1201) was raised by SoCo's own maintainer (see below) | Shipped as `link --from-household` (`src/sonos/stored.rs`); the token is the same credential a device link mints, good only for public SMAPI search and browse |
| **The LAN Control API** (WebSocket, port 1443) | Answers any controller on the LAN; x2rock sends the community's placeholder API key, not a portal-issued one | §2(b) requires a separate licence to publish a LAN-based app, for those bound by the terms | The core of x2rock; shipped |
| **Households with Authentication on** | x2rock presents a token from the person's own developer integration (verified 2026-09-27) | Uses the documented OAuth flow with the person's own key (§1(b)–(c)); it ties LAN use to an account, so §2(b) applies to that person | Shipped, opt-in |

## The household-token salt

The key protecting the stored tokens is `md5(iv + md5(householdId + SALT))`, with a fixed 16-byte
`SALT`. The value itself originates in Sonos software:

- **svrooij/sonos-net** (commit 76bbb86, 2026-04-09) carries it with the comment "This key was
  extracted from the Sonos app" — the earliest public copy found. There may be others.

For a service whose AppLink flow `link` can complete, the token it mints is an
**exact match** for the one the salt recovers — the same `{token, key}` SMAPI `loginToken`, for the same
account. The salt is a shortcut to a credential the device **could mint for itself**, spent only on SMAPI
search and browse (and play where the service allows), and it **never touches the Sonos cloud**.

## Facts worth keeping, from documented or observable behaviour

- Sonos's OAuth is a confidential-client flow: the client secret is needed on the code exchange
  and on every refresh, with no PKCE path. Anyone holding a sign-in holds their secret locally.
- A service's own SMAPI endpoint serves a third party what that service chooses to.
  - Spotify, Deezer, TuneIn, SiriusXM and Sonos Radio publish a browse endpoint that matches the
    app.
  - Amazon answers its Home with 401, and Apple refuses the household token.
  - YouTube Music answers 403 at Google's API project gate. That is Google's restriction, not a
    Sonos term.
