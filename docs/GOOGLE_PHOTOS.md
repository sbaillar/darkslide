# Export to Google Photos

An export recipe can also upload its JPEG or HEIC files straight into your Google Photos library, into an album you pick or a new one. The files are still written to the recipe's folder first, so the upload is an extra copy.

## Using it

1. Google Photos is an optional integration: turn it on in **Settings ▸ Plugins ▸ Integrations** first (turning it off later keeps your sign-in).
2. Open **Settings ▸ Google Photos** and click **Sign In with Google…**. Choose your account and allow both permissions. When the page says "Done", close it.
3. Select photos and press ⌘E. Pick or add a JPEG or HEIC recipe, and under **Google Photos** turn on **Also upload to Google Photos**.
4. Choose an album Luxtable made before, or type a name. Luxtable adds to its own album with that name, or creates it.
5. Click **Export**. The sheet shows each photo as it uploads. At the end it shows how many went up, and which failed and why.

The photo's caption (or its title, if it has no caption) becomes the description in Google Photos. Uploads count toward your Google Account storage.

Release builds have Luxtable's Google client built in, so signing in is all you need to do. A build without one (your own build, or a fork) shows setup steps in Settings instead. See [Supplying the OAuth client](#supplying-the-oauth-client).

## What Luxtable can do with your account

| Scope | What it allows |
|---|---|
| `photoslibrary.appendonly` | Upload photos, create albums, and add photos to albums Luxtable created |
| `photoslibrary.readonly.appcreateddata` | List the albums and photos Luxtable created (for the album picker) |

Since 31 March 2025 the Google Photos Library API only lets an app see and change what it created itself. Luxtable can't read, change or delete anything else in your library. It can only add photos to albums it created, so the album picker lists only those.

The refresh token Google issues is kept in your login Keychain (service `app.luxtable.Luxtable.google-photos`), never in preferences or files. If you use your own client, its secret is kept there too. Signing out revokes the token at Google and deletes it.

## Supplying the OAuth client

The client ID is **never committed** to this repository. `scripts/build-app.sh` builds it into the app's Info.plist (`DSGoogleClientID`, `DSGoogleClientSecret`). It reads the values from, in order:

1. the environment variables `LT_GOOGLE_CLIENT_ID` and `LT_GOOGLE_CLIENT_SECRET`, or
2. `secrets/google-oauth.json`. The `secrets/` folder is git-ignored. The file can be the JSON that Google Cloud Console downloads for a Desktop client (`{"installed": {"client_id": …, "client_secret": …}}`), or a plain `{"client_id": …, "client_secret": …}`.

Without either, the app is built with no client. Each user can then add their own in **Settings ▸ Google Photos**. Even when a client is built in, **Use your own Google OAuth client (advanced)** overrides it.

- **Official release builds** use the project owner's client, supplied as above on the release machine.
- **Contributors and forks** create their own client (next section). Then they either drop the downloaded JSON at `secrets/google-oauth.json` before running `scripts/build-app.sh`, or paste the ID and secret into Settings.

Google's guidance for installed apps is that a Desktop client's secret isn't treated as confidential. Any app that ships it can be inspected to read it, which is how all installed apps work. It is still kept out of the repository.

## Create an OAuth client

You need a Google account. It takes about five minutes.

1. Open [Google Cloud Console](https://console.cloud.google.com/) and create a project (for example "Luxtable uploads"), or pick one you already have.
2. Go to **APIs & Services ▸ Library**, search for **Photos Library API**, and click **Enable**.
3. Go to **Google Auth Platform** (APIs & Services ▸ OAuth consent screen) and click **Get started**:
   - App name: anything, for example "Luxtable". User support email: yours.
   - Audience: **External**.
   - Contact information: your email. Accept the policy and click **Create**.
4. In **Google Auth Platform ▸ Data Access**, click **Add or remove scopes** and filter for "photoslibrary". Tick these two scopes, then click **Update** and **Save**:
   - `.../auth/photoslibrary.appendonly`
   - `.../auth/photoslibrary.readonly.appcreateddata`

   The console shows each scope's classification (sensitive or restricted) in this list.
5. In **Google Auth Platform ▸ Clients**, click **Create client**:
   - Application type: **Desktop app**. Google allows only loopback redirects (`http://127.0.0.1:<port>`) for Desktop clients, and Luxtable uses one. There's nothing to register.
   - Name: anything. Click **Create**.
   - Download the JSON, or copy the **Client ID** (it ends in `.apps.googleusercontent.com`) and the **Client secret** (it starts with `GOCSPX-`).
6. Supply the client as described in [Supplying the OAuth client](#supplying-the-oauth-client).
7. Choose the publishing status. This decides how long a sign-in lasts, as the next section explains.

## Keeping the sign-in from expiring

What Google's documentation says (checked 2026-10-10):

- **"Testing" status:** a project with the External user type and "Testing" publishing status "is issued a refresh token expiring in 7 days". The only exception is an app that requests nothing but the name, email and profile scopes, and Luxtable's Photos scopes aren't among them. Only listed test users can sign in. So in Testing you sign in again every week. When the token has expired, an export says "sign in again in Settings ▸ Google Photos".
- **"In production" status:** Google states no 7-day expiry for production apps. Refresh tokens last until the user revokes them, or until Google's general limits apply (for example, a token unused for six months, or too many tokens for one account and client).
- **Unverified app:** the Google Photos API docs say an app that uses the Photos APIs "must pass the OAuth verification review", and that seeing **unverified app** at sign-in means you must submit a verification request to remove it. Until the app is verified, users see the "Google hasn't verified this app" screen and click through **Advanced ▸ Go to … (unsafe)**. The project also has a cap of **100 users in total**, which "applies over the entire lifetime of the project, and it cannot be reset or changed". Google's help adds that an app "for your personal use (fewer than 100 users)" can keep running without verification.
- **Scope classification:** Google's Photos docs don't say whether `photoslibrary.appendonly` is "sensitive" or "restricted". The **Data Access** page in your project shows the classification when you add the scope, so check there. Either way, the Photos docs require verification before the unverified-app warning goes away.

**A non-expiring sign-in for your own use (no verification):**

1. Create the client as above.
2. In **Google Auth Platform ▸ Audience**, click **Publish app** and confirm. The status changes from "Testing" to "In production".
3. Sign out in Luxtable (Settings ▸ Google Photos), then sign in again. A token issued while the app was in Testing keeps its 7-day expiry.
4. At "Google hasn't verified this app", click **Advanced**, then **Go to Luxtable (unsafe)**, and allow both permissions.

This lasts until you sign out or revoke Luxtable at https://myaccount.google.com/permissions. It counts toward the 100-user cap.

**For an official release used by the public**, the owner's project needs to be **In production and verified** by Google (the OAuth verification review the Photos docs require). Without verification, every user sees the unverified-app screen, and sign-ins stop after the 100th user.

## How it works

- `crates/lt-gphotos` holds the protocol and has no network code of its own. Every request goes through one seam, `Transport`: `URLSession` in the app, an in-process mock Google server in the tests.
- Sign-in is OAuth 2.0 for installed apps with PKCE (S256). Luxtable listens on `127.0.0.1` at a free port while the consent page is open (`ASWebAuthenticationSession`), takes the code from the browser's redirect, and checks the `state` value.
- Uploads use the resumable protocol in 8 MB chunks. A chunk that fails is retried from the offset the server reports. A 429 waits at least 30 seconds. 5xx and network errors back off exponentially (1, 2, 4… seconds, up to 6 tries). An expired access token is refreshed once and the request is resent.
- Uploaded files are added with `mediaItems:batchCreate`, 50 at a time, one call after another (as Google asks). Each file's result is checked, so one bad file doesn't fail the others.
- Limits: JPEG or HEIC only, at most 200 MB per photo, descriptions up to 1,000 characters, album titles up to 500.
