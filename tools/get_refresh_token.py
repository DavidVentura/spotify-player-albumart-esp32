#!/usr/bin/env python3
"""Runs Spotify's authorization-code flow once and prints a refresh token.

Register http://127.0.0.1:8888/callback as a redirect URI on the app first.
Usage: SPOTIFY_CLIENT_ID=... SPOTIFY_CLIENT_SECRET=... ./get_refresh_token.py
"""

import base64
import http.server
import json
import os
import secrets
import sys
import urllib.parse
import urllib.request
import webbrowser

REDIRECT_URI = "http://127.0.0.1:8888/callback"
SCOPE = "user-read-currently-playing"


def wait_for_code(expected_state: str) -> str:
    received: dict[str, str] = {}

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
            received.update({key: values[0] for key, values in query.items()})
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"Done, you can close this tab.")

        def log_message(self, *_args) -> None:
            pass

    redirect = urllib.parse.urlparse(REDIRECT_URI)
    with http.server.HTTPServer((redirect.hostname, redirect.port), Handler) as server:
        server.handle_request()

    if "error" in received:
        sys.exit(f"Authorization failed: {received['error']}")
    if received.get("state") != expected_state:
        sys.exit("Authorization failed: state mismatch")
    return received["code"]


def exchange(client_id: str, client_secret: str, code: str) -> dict:
    credentials = base64.b64encode(f"{client_id}:{client_secret}".encode()).decode()
    request = urllib.request.Request(
        "https://accounts.spotify.com/api/token",
        data=urllib.parse.urlencode(
            {"grant_type": "authorization_code", "code": code, "redirect_uri": REDIRECT_URI}
        ).encode(),
        headers={
            "Authorization": f"Basic {credentials}",
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    with urllib.request.urlopen(request) as response:
        return json.load(response)


def main() -> None:
    client_id = os.environ["SPOTIFY_CLIENT_ID"]
    client_secret = os.environ["SPOTIFY_CLIENT_SECRET"]
    state = secrets.token_urlsafe(16)
    url = "https://accounts.spotify.com/authorize?" + urllib.parse.urlencode(
        {
            "client_id": client_id,
            "response_type": "code",
            "redirect_uri": REDIRECT_URI,
            "scope": SCOPE,
            "state": state,
        }
    )
    print(f"Open this URL if no browser appears:\n{url}\n", file=sys.stderr)
    webbrowser.open(url)

    tokens = exchange(client_id, client_secret, wait_for_code(state))
    print(f"SPOTIFY_REFRESH_TOKEN={tokens['refresh_token']}")


if __name__ == "__main__":
    main()
