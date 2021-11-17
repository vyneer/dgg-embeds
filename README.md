# dgg-embeds

Collects embeds from dgg, gets some data from them and puts them into an SQLite DB.

---

## How to deploy

1. Obtain the necessary API tokens from [Twitch](https://dev.twitch.tv).
2. ```cp .env.example .env```
3. Add your API keys/tokens in the ```.env``` file.
4. ```cargo build --release```
5. ```dgg-embeds```

or, if you wanna use Docker

4. ```docker build -t dgg-embeds .```
5. ```docker run --env-file .env -it dgg-embeds```