# Knihovny.cz metadata provider

BookOrbit metadata from [Knihovny.cz](https://www.knihovny.cz), the portal of Czech libraries,
through its public API. It covers Czech and Slovak books and the Czech translations of foreign
ones, as Czech libraries catalogue them: titles and subtitles, authors, series and position,
descriptions, covers, publishers, years, page counts, ISBNs, language, genres, and for audiobooks
their narrators and running time. It runs beside BookOrbit as a small container.

Rust: axum, tokio, reqwest with rustls, serde and regex, the same stack as the published
[BookOrbit providers](https://github.com/bookorbit/bookorbit-metadata-providers). The image is a
static musl binary on `scratch`, under 3 MB.

## Run

Beside BookOrbit, on the same Docker network:

```yaml
services:
  knihovnycz:
    image: ghcr.io/fhavrlent/knihovnycz-provider:0.1.0
    restart: unless-stopped
```

Then add `http://knihovnycz:8080` in BookOrbit under **Settings > Metadata > Providers > External
providers**; the [BookOrbit guide](https://github.com/bookorbit/bookorbit/blob/main/docs/METADATA_PROVIDERS.md)
has the steps. No account or key is needed. To build it yourself:
`docker build -t knihovnycz-provider .`, or with a Rust toolchain, `cargo run --release`.

| Variable | Default | |
|---|---|---|
| `PORT` | `8080` | |
| `AUTH_TOKEN` | unset | When set, `/search` and `/book/{id}` require this exact `Authorization` header |
| `KNIHOVNY_SITE` | `https://www.knihovny.cz` | Where the API is called; links in answers always point at knihovny.cz |
| `USER_AGENT` | `bookorbit-knihovnycz-provider/0.1.0` | |
| `DEADLINE_MS` | `8000` | How long a search or lookup may take, covers included |

The provider sends knihovny.cz at most two requests at a time, at least 250 ms apart, whatever its
clients ask for. When it answers `429`, the provider sends it nothing more until its `Retry-After`
has passed, and answers `429` itself meanwhile.

What it sends to knihovny.cz: the title, the author's surname or the ISBN of each book BookOrbit
asks about, and the record ids and ISBNs it looks up. It logs request paths and failures, never
titles.

## Endpoints

| Route | |
|---|---|
| `GET /provider/info` | Key `knihovnycz`, books, audiobooks and comics, lookup, ISBN search, a URL template for record links, icon and colour. No cover shape: print covers are portrait, audiobook covers square |
| `GET /search?query=&author=&isbn=&mediaType=&limit=` | `limit` defaults to 5, at most 10 |
| `GET /book/{id}` | A record id such as `nkp.NKC01-003423268`, or a knihovny.cz record address. The answer carries the id that was asked for |

A search is one or two API requests, then a cover check per book: about one to three seconds.

Status codes: `400` neither a query nor a valid ISBN, `401` bad token, `404` unknown id, `429` with
`Retry-After` when knihovny.cz throttles, `502` when it fails, times out or answers something the
provider does not understand. An empty list only ever means the API said it found nothing. A
search whose cover checks did not all finish answers `200` with `"partial": true`, so BookOrbit
does not cache it as complete.

## Mapping

- **ISBN search** comes first when BookOrbit sends an ISBN. The API's search for it matches other
  fields too, so only an edition that lists the ISBN is the answer (an ISBN-10 and its ISBN-13
  count as the same). When an edition lists several ISBNs, the one searched is reported.
  Placeholder ISBNs of zeros, which badly tagged files carry, are not searched.
- **Title search** asks for the title and the author's surname together, which the catalogue
  matches precisely, and for the title alone when that finds nothing by the author or with the
  title.
- **One answer per edition.** The catalogue unites many libraries, so an edition arrives once per
  library that holds it. Results are ranked: the author and title asked for, the medium asked for
  (an audiobook search puts audiobooks first), a Czech or Slovak edition (the catalogue holds
  translations too: "Krakatit" brings German editions), a record with a description, the National
  Library's own record, an ISBN. Copies of an edition then fold into the best-placed one.
- **Ids** are the catalogue's record ids, such as `nkp.NKC01-003423268`: a library's code, a dot
  and its own id.
- **Title and series** come from the catalogue record's title parts: "Zaklínač. I., Poslední
  přání" is the title "Poslední přání" in the series "Zaklínač", part 1. A subtitle ("Sapiens :
  úchvatný i úděsný příběh lidstva") is split off. A series is also read from the series statement
  when its name begins the title; publishers' editions written there ("Leonardo fantasy ; 8") are
  not taken for a series.
- **Authors** without life dates ("Andrzej Sapkowski, 1948-" is "Andrzej Sapkowski").
- **Language** from the record's fixed-length data: `cze` is `cs`, `slo` is `sk`, also `en`, `de`,
  `pl`, `fr`, `es`, `it`, `ru`, `hu`.
- **Media type** is `audiobook` for the catalogue's audio formats, `comic` for the genre
  "komiksy", else `book`.
- **Audiobooks** add narrators (people with a narrator's role, else the performer note, "Čte …")
  and the running time ("123100" is 12 h 31 min).
- **Genres** are the Czech genre headings ("fantasy povídky"), without headings that name a format
  ("audioknihy", "kompaktní disky").
- **Cover** is the site's large cover for the edition's ISBN, else its barcode (EAN), which
  audiobooks often have instead. The site answers a small placeholder for a book without a cover;
  the provider checks each cover and leaves out placeholders.

Known gaps:

- No community ratings.
- Few audiobook records: an audiobook search often finds only the print editions.
- Records come from many libraries and vary: some lack a description, some have names written
  oddly ("Michail Afanas‘jevič Bulgakov").
- Series only when the record's title or series statement names it.

## Test

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

Or in a container, with no local toolchain:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.90-alpine sh -c 'apk add --no-cache musl-dev && cargo test'
```

The tests run offline against a stand-in for the API. The fixtures in `testdata/` are synthetic
API answers in knihovny.cz's JSON and MARCXML shape, with invented books, people and texts: a
search with several libraries' copies of one edition, a translation and an audiobook, and records
for a print edition, an audiobook with narrators by role and one with a performer note, a Slovak
edition, an edition with three ISBNs, a subtitle and a comic. The stand-in also plays the API's
failures: an answer it does not understand, an error, a `429`, a slow answer, a failing or slow
cover, an unknown id.

Live tests check the real API: well-known books keep their title, series, language, pages, cover,
genres, narrators and running time; ISBN searches answer only the edition that lists the ISBN;
Slovak editions and subtitles; lookups and unknown ids. `cargo test` skips them; run them with:

```bash
cargo test live -- --ignored --test-threads=1
```

The `Publish` workflow runs the offline and live tests before it builds an image, so a release that
fails them is never published.
