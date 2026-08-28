# codex-api-wrapper (PoC)

ChatGPT-Abo über die **offiziellen Codex-Crates** — als Bibliothek, CLI und
lokale REST-API. Stand: PoC. OpenAI-kompatibel (`/v1/chat/completions`,
`/v1/responses`), mit Metriken unter `/metrics`, kein Prozess-Pool (braucht es
nicht, siehe [MESSUNGEN.md](MESSUNGEN.md)).

Schwesterprojekt zu `claude-api-wrapper`. Der Kontext steht in
[../KONTEXT-HARNESS.md](../KONTEXT-HARNESS.md) — dieses Repo setzt dessen §8 um.

## Warum der Name

Das Abo heißt ChatGPT, die CLI heißt Codex. Der Endpoint ist Codex-spezifisch
(`chatgpt.com/backend-api/codex`, nur Responses-Wire-API, Codex-`originator`), und
das Kontingent hängt am Codex-Produkt. Der Name sagt, was angesprochen wird.

> Die ursprüngliche Begründung lautete zusätzlich „der erzwungene
> Codex-Systemprompt in jedem Request". Das hat sich als **falsch** erwiesen —
> siehe [MESSUNGEN.md](MESSUNGEN.md) §1. Der Endpoint erzwingt keine
> `instructions`.

## Was das ist — und was nicht

**Ist:** ein CLI, das den offiziellen ChatGPT-OAuth-Flow gegen `codex-login` fährt
und danach Responses-Requests gegen `chatgpt.com/backend-api/codex` absetzt, mit
frei wählbaren `instructions` und `tools`.

**Ist nicht:** ein Proxy, der `~/.codex/auth.json` ausliest und selbst gegen das
Backend postet. Das ist der Pfad, den KONTEXT-HARNESS.md §8.1 ausdrücklich
ausschließt. Hier läuft Auth vollständig über die öffentliche API von
`codex-login` — kein eigener Token-Endpoint, kein selbstgeschriebenes `auth.json`.

### Einordnung

Das ist **Stufe 3** aus §8.4 (`codex-core` als Library), die Stufe mit der
dünnsten ToS-Deckung. Der Zuschnitt mildert das an einer Stelle, die zählt: der
Login läuft durch den Originalcode mit der offiziellen `client_id` und
`originator=codex_cli_rs`. Was dieses Repo trotzdem nicht behauptet, ist eine
Freigabe — §8.2 bleibt gültig, insbesondere die Account-Klausel (lokal für sich
selbst = kein Sharing; offener Port für Kollegen = Sharing).

## Aufbau

Die Bibliothek ist das Produkt; das Binary ist eine dünne Hülle darum.

| Datei | Inhalt |
|---|---|
| [src/wire.rs](src/wire.rs) | das Vokabular nach außen — **provider-neutral** |
| [src/client.rs](src/client.rs) | Codex-Typen → `wire`; hier endet das Wissen über `codex-api` |
| [src/auth.rs](src/auth.rs) | eigener CODEX_HOME, OAuth-Flow, `AuthManager` |
| [src/models.rs](src/models.rs) | der Modellkatalog, projiziert und korrigiert |
| [src/openai_chat.rs](src/openai_chat.rs) | `wire` → Chat Completions |
| [src/openai_responses.rs](src/openai_responses.rs) | `wire` → Responses-API |
| [src/limits.rs](src/limits.rs) | Kontingent-Gruppen, aus Headern und Poll-API in eine Form |
| [src/metrics.rs](src/metrics.rs) | was der laufende Prozess beobachtet hat |
| [src/serve.rs](src/serve.rs) | die REST-API |
| [src/main.rs](src/main.rs) | CLI |

Die tragende Regel: **oberhalb von `client` taucht kein Codex-Typ mehr auf.** Wie
das Backend erreicht wird, bleibt damit austauschbar, ohne dass Konsumenten es
merken. `wire` spricht bewusst nicht das Vokabular eines bestimmten Abnehmers —
sonst wäre der Daemon an einen gekettet.

Der tragende Befund der Recherche: **`codex-core` wird nicht gebraucht.** Login-
und Responses-Pfad sind im Upstream sauber von der Agent-Maschinerie getrennt.
Eingebunden sind nur `codex-login`, `codex-api`, `codex-http-client`,
`codex-config`, `codex-protocol`.

Credentials liegen in `~/.codex-api-wrapper` (überschreibbar per
`CODEX_WRAPPER_HOME`), **nicht** in `~/.codex`. Eine echte Codex-Installation
wird weder gelesen noch angefasst.

## Bauen

Setzt einen Checkout von `openai/codex` als Schwesterordner unter `../codex`
voraus.

```bash
git clone --depth 1 https://github.com/openai/codex.git ../codex
cargo build
```

Zwei Stolpersteine, beide schon gelöst, aber wartungsrelevant:

- **`[patch.crates-io]` in [Cargo.toml](Cargo.toml)** muss mit
  `../codex/codex-rs/Cargo.toml` synchron bleiben. Cargo vererbt `[patch]` nicht
  über Workspace-Grenzen, und `codex-api` braucht ein `proxy`-Feature, das nur
  OpenAIs `tokio-tungstenite`-Fork hat. Ohne den Block scheitert schon die
  Abhängigkeitsauflösung.
- **Toolchain:** `../codex/codex-rs/rust-toolchain.toml` pinnt 1.95.0. Ohne
  `rustup` greift der Pin nicht; mit System-Rust 1.94.1 baut es. Mit `rustup` im
  PATH wird 1.95.0 nachinstalliert.

Beides ist der in §8.4 vorhergesagte „praktische Preis" — bei jedem
Upstream-Update gegenprüfen.

## Benutzung

```bash
codex-api-wrapper login              # OAuth mit lokalem Callback-Server
codex-api-wrapper login --device     # Device-Code — ohne Browser, ohne Port
codex-api-wrapper whoami             # Plan, Account-ID, Token-Zustand
codex-api-wrapper models             # welche Modelle gibt das Abo frei
codex-api-wrapper ask "..."          # Responses-Request
codex-api-wrapper serve              # lokale REST-API (s.u.)
codex-api-wrapper logout             # Token widerrufen
```

Der **Device-Code-Flow** ist für ChatGPT-Accounts freigeschaltet (gemessen). Er
braucht keinen Browser auf derselben Maschine und keinen Callback-Port — für
entfernte Hosts und Container der richtige Weg, siehe [DEPLOY.md](DEPLOY.md).
`login --device --probe` fragt nur einen Code an, ohne etwas anzumelden.

## Die REST-API (`serve`)

Ein Prozess, beliebig viele gleichzeitige Requests. Gemessen: zehn parallele
Turns ohne Vermischung.

```bash
codex-api-wrapper serve --listen unix:/run/codex/sock          # lokal
codex-api-wrapper serve --listen 0.0.0.0:8080 --api-keys keys.txt   # Netz
```

JSON-Request-Bodies sind auf **32 MiB** begrenzt. Das hebt Axums sonst
greifendes 2-MiB-Default auf und entspricht `client_max_body_size 32m` im
vorgeschalteten nginx. Wer den Proxy-Wert ändert, muss diese Grenze mitprüfen.

### Pfade — ein Präfix, eine Entscheidung

| Präfix | Inhalt | exponierbar |
|---|---|---|
| `/v1/*` | OpenAI-kompatibel — `models`, `chat/completions`, `responses` | ja |
| `/wire/v1/*` | eigenes Vokabular: `info`, `whoami`, `usage`, `models`, `responses` | ja |
| `/health`, `/ready`, `/metrics` | Betrieb, Sonden, Metriken | **nein** |

Ein Reverse-Proxy kommt so mit einer Regel pro Oberfläche aus, und was nicht
freigegeben ist, bleibt automatisch drinnen:

```nginx
location /v1/      { proxy_pass http://127.0.0.1:8080; }
location /wire/v1/ { proxy_pass http://127.0.0.1:8080; }
location /         { return 404; }
```

`/v1` liegt auf der Wurzel, weil manche Clients einen Host nehmen und
`/v1/chat/completions` selbst anhängen. `/wire/v1` ist von Anfang an versioniert,
weil `wire::Event` sich noch bewegt.

### Womit spreche ich? — `/wire/v1/info`

```json
{ "service": "codex-api-wrapper", "version": "1.4.1" }
```

Mehr nicht, und das ist Absicht:

- **Die Vertragsversion steht im Pfad.** `/wire/v1` sagt bereits, welches Format
  gilt; ein Feld dafür wäre eine zweite Wahrheit über dieselbe Sache.
- **`version` ist die Release-Version**, direkt aus `Cargo.toml` (`env!`), damit
  sie gar nicht erst von der Kiste abweichen kann, die sie beschreibt. Wer sein
  Verhalten daran festmacht, macht es am Falschen fest — dafür ist die
  Pfadversion da. Sie taugt für Logs, Fehlerberichte und die Frage, ob ein
  Deployment schon die neue Fassung fährt.
- **Fähigkeiten stehen dort, wo sie gelten:** Modelle in `/wire/v1/models`,
  Kontingent in `/wire/v1/usage`, Anmeldung in `/wire/v1/whoami`. Ein vierter
  Ort, der alles zusammenfasst, wäre eine Kopie, die veraltet.

Antwortet ohne Rückfrage beim Backend und unabhängig vom Anmeldezustand: wer
wissen will, womit er spricht, soll das auch dann erfahren, wenn der Daemon
gerade nicht arbeiten kann — dafür ist `/ready` da.

### Der Modellkatalog — `/wire/v1/models`

Eine schlanke Projektion in unserem Vokabular, kein Durchreicher. Die Rohobjekte
tragen je ein `instructions_template` von ~17 KB — 169 KB für eine Liste, die
meist einen Auswahldialog füllt — und der Katalog stimmt an mehreren Stellen nicht
mit dem Verhalten überein. Beides gehört nicht in den Vertrag eines Konsumenten.
Wer die Rohdaten braucht, hängt **`?raw=true`** an.

```json
{
  "models": [
    {
      "id": "gpt-5.6-sol",
      "model": "gpt-5.6-sol",
      "display_name": "GPT-5.6-Sol",
      "description": "Latest frontier agentic coding model.",
      "hidden": false,
      "input_modalities": ["text", "image"],
      "default_context_length": 272000,
      "max_context_length": 872000,
      "reasoning": {
        "levels": ["none", "low", "medium", "high", "xhigh", "max"],
        "default": "low",
        "summary_supported": true
      }
    }
  ]
}
```

**`id` und `model` sind getrennt**, auch wenn sie hier gleich sind: `id` ist der
Schlüssel, den ein Konsument wählt, `model` der Name, der ans Backend geht. Auf
der OpenAI-Oberfläche laufen sie auseinander (s.u.).

#### Die beiden Kontextwerte

| Feld | Herkunft | Bedeutung |
|---|---|---|
| `default_context_length` | `context_window` | womit Codex selbst fährt — steht bei **allen** Modellen auf 272 000, außer Spark (128 000) |
| `max_context_length` | `max_context_window`, korrigiert | die tatsächlich nutzbare Obergrenze |

Eine Kompaktierungsschwelle liefern wir **nicht** mit. Codex nimmt 90 % seines
Fensters (`ModelInfo::auto_compact_token_limit`), aber wann ein Konsument
kompaktiert, ist seine Politik und nicht unsere Auskunft — die beiden Zahlen
oben reichen, um sie zu bilden.

**Der Wrapper sendet kein Kontextfeld ans Backend.** Beide Werte sind reine
Buchhaltung für den Konsumenten; nichts hindert ihn daran, `max_context_length`
auszuschöpfen. Gemessen (MESSUNGEN.md §16): 871 963 Tokens gehen auf allen
Modellen der 872k-Gruppe durch, obwohl Codex bei 272 000 bliebe.

**Korrekturtabelle.** Eine Messung darf eine zu hohe Angabe **deckeln, nie
anheben**. Deshalb genau ein Eintrag:

| Modell | Katalog | ausgeliefert | Grund |
|---|---:|---:|---|
| `gpt-5.4` | 1 000 000 | **872 000** | ~950 000 wurde abgelehnt; 872 000 ist der von derselben Stufe deklarierte und auf vier Modellen bestätigte Wert |

Alle übrigen behalten ihre Katalogangabe: die 872k-Gruppe ist an ihrem Maximum
geprüft, `gpt-5.5` hält seine 272 000 nachweislich ein, und `gpt-5.4-mini` wie
Spark haben `max == default`, also keinen Spielraum.

#### Reasoning

`reasoning.levels` ist die **korrigierte** Liste, nicht die des Katalogs — `ultra`
entfernt, `none` ergänzt außer bei `gpt-5.3-codex-spark` (Messdaten unter
„OpenAI-kompatibel" und in MESSUNGEN.md §14.1). `default` ist der Vorgabewert des
Backends und bleibt unangetastet; er kann außerhalb von `levels` liegen, wenn das
Backend seine eigene Liste verletzt.

#### Was gefiltert wird und was nicht

**Versteckte Modelle** (`visibility != "list"`, beim Abo `gpt-reserve` und
`codex-auto-review`) fehlen standardmäßig; `?include_hidden=true` zeigt sie.

**`supported_in_api` wird weder durchgereicht noch als Filter benutzt.** Spark
meldet dort `false` und funktioniert nachweislich — mehrfach mit 200 und echtem
Verbrauch gefahren. Danach zu filtern würde ausgerechnet das einzige Modell mit
eigenem Kontingent entfernen.

Nicht enthalten ist `max_output_tokens`: ein solches Feld gibt es im Katalog
nicht, und ein dauerhaft leerer Schlüssel wäre Rauschen.

**Ebenfalls bewusst nicht enthalten: die Kontingent-Gruppe eines Modells.** Ein
Feld wie `"limit_group": "codex_bengalfox"` wäre verlockend — nur existiert die
Beziehung nirgends deklariert (MESSUNGEN.md §13.7), sie wäre also entweder ein
Namensabgleich oder eine gemessene Tabelle. Beides würde hier **veralten, ohne
dass etwas fehlschlägt**: wird ein Modell künftig anders bemessen, bleibt die
Angabe still falsch.

Ein Konsument kann es besser, weil er Zustand hat. Er lädt ohnehin beide Listen —
`display_name` aus dem Katalog, `limit_name` und `metered_feature` aus
`/wire/v1/usage` — und bekommt mit **jedem Turn** `limits.active_group` geliefert.
Damit lernt er die tatsächliche Zuordnung aus dem Betrieb und merkt selbst, wenn
sie sich ändert. Der Wrapper liefert die Bausteine, nicht die Schlussfolgerung.

### OpenAI-kompatibel

`GET /v1/models` liefert die Standardform (`object: "list"`, `id`, `object`,
`created`, `owned_by`) plus drei nützliche Zusatzfelder, die Clients ignorieren
dürfen: `display_name`, `context_length`, `reasoning_levels`.

**Schlanke Projektion, kein Durchreichen** — 1 KB statt 169 KB. Die
Backend-Objekte tragen je ein `instructions_template` von ~17 KB; wer die
Rohdaten will, nimmt `/wire/v1/models`.

**Diese Liste ist meinungsstark, `/wire/v1/models` ist vollständig.** Ein
OpenAI-Client erwartet pro Modell **eine** Kontextgröße und rechnet seine
Kompaktierung selbst darauf. Der Katalog kennt aber zwei Werte — was Codex fährt
und was möglich ist. Ausgedrückt wird das hier als zweiter Eintrag:

| `id` | `model` (ans Backend) | `context_length` |
|---|---|---:|
| `gpt-5.6-sol` | `gpt-5.6-sol` | 272 000 |
| `gpt-5.6-sol:long` | `gpt-5.6-sol` | 872 000 |

**Beide erzeugen byte-gleiche Requests.** Der Wrapper sendet kein Kontextfeld ans
Backend; der Unterschied ist ausschließlich die Zahl, mit der der Client
budgetiert. Die Variante ist eine **Budgetempfehlung**, keine Fähigkeit, die wir
zuschalten. Der Suffix wird vor dem Senden abgeschnitten — deshalb sind `id` und
`model` im Wire-Katalog getrennt.

**Nur für `gpt-5.6-sol`.** Berechtigt wären sechs Modelle (`max > default`), aber
eine Variante verdoppelt einen Eintrag im Modellwähler für null funktionalen
Unterschied. `sol` ist das Arbeitspferd; für alles andere liefert
`/wire/v1/models` `max_context_length`, und wer terra mit 872 000 fahren will,
budgetiert das selbst. Eine weitere Variante ist eine Zeile, sobald es einen Fall
dafür gibt.

Ein 872k-Turn kostet übrigens rund das Dreifache eines 272k-Turns an Kontingent —
~250 statt ~800 Turns pro Woche (MESSUNGEN.md §16.5). Die Variante weist eine
Fähigkeit aus; ob man sie ausreizt, bleibt eine Budgetentscheidung des Clients.

**`reasoning_levels` wird korrigiert, nicht durchgereicht.** Der Katalog des
Backends widerspricht seiner eigenen Prüfung, und zwar in beide Richtungen:
`ultra` steht bei `gpt-5.6-sol` und `-terra` in `supported_reasoning_levels`,
jeder Request damit bekommt `400 Invalid value: 'ultra'` — gemessen am
2026-08-27 auf drei Modellen. Umgekehrt fehlt `none`, das überall angenommen wird
und das Denken abschaltet (`reasoning_output_tokens: 0`). Ein Modellwähler, der
sich direkt aus dem Katalog speist, böte also einen Wert an, der nicht wählbar
ist, und verschwiege den schnellsten. Deshalb fliegt `ultra` raus und `none`
kommt dazu — letzteres **außer bei `gpt-5.3-codex-spark`**, dem einzigen der neun
Modelle, das `none` ablehnt. Alle drei Listen stehen als Konstante mit Messdatum
in [src/models.rs](src/models.rs). Nicht ergänzt wird `minimal` — das Backend nennt
es in seiner allgemeinen Fehlermeldung, lehnt es für diese Modelle aber
ausdrücklich ab.

**`context_length`** ist der korrigierte `max_context_length` aus dem
Wire-Katalog für `:long`-Einträge und `default_context_length` für die
Basiseinträge. Die Korrekturtabelle steht oben unter „Der Modellkatalog".

Zwei Entscheidungen, die dort dokumentiert sind: `created: 0`, weil das Backend
kein Datum nennt und ein wanderndes `now()` schlimmer wäre als eine ehrlich
falsche Konstante. Und Modelle mit `visibility: hide` (beim Abo: `gpt-reserve` und
`codex-auto-review`) fehlen standardmäßig — `?include_hidden=true` zeigt sie.

### Zwei Inferenz-Endpunkte — welchen nehmen?

| | `/v1/chat/completions` | `/v1/responses` |
|---|---|---|
| Reichweite | größer, jeder OpenAI-Client | Clients mit Responses-Unterstützung |
| Thinking | `reasoning_content`-Deltas | typisiertes `reasoning`-Item mit `summary`-Parts |
| Text **und** Tool-Call im selben Turn | nicht darstellbar | ja |
| Reasoning-Replay | nein | ja, `encrypted_content` reist mit |
| Abbruch | `finish_reason: "length"` | `status: "incomplete"` + `incomplete_details` |
| Nicht-Streaming | ja | ja |

Beide fahren dieselbe Pipeline; die Übersetzung liegt in
[src/openai_chat.rs](src/openai_chat.rs) bzw.
[src/openai_responses.rs](src/openai_responses.rs).

**Für Open WebUI:** Verbindung anlegen, *API-Typ* auf `Responses` stellen — der
Client postet dann auf `<base-url>/responses`. Mehr braucht es nicht: die
Denk-Summary wird für jeden Turn angefordert, unabhängig davon, ob ein Client
einen Effort mitschickt.

Wer *wie viel* gedacht wird steuern will, setzt zusätzlich
*Erweiterte Parameter → Reasoning Effort* (pro Modell, pro Chat oder als
Admin-Default) auf einen Wert aus `reasoning_levels`. Beide Schreibweisen werden
angenommen: `reasoning.effort` nach Spezifikation und das
Chat-Completions-`reasoning_effort`, das Open WebUI beim Umbauen der Nutzlast
unangetastet stehen lässt.

Bei den kleinen Modellen bleibt der Denkblock ohne Effort leer — `gpt-5.4-mini`
denkt von sich aus gar nicht (`reasoning_tokens: 0`), da ist nichts zu berichten.
Ein Effort schaltet es ein.

### Die Responses-API (`/v1/responses`)

`input` nimmt einen String oder fertige Items (`message` mit
`input_text`/`output_text`/`input_image`, `function_call`,
`function_call_output`, `reasoning`). Dazu `instructions`, `tools` in der flachen
Responses-Form, `tool_choice`, `parallel_tool_calls`, `reasoning.effort` und
`stream`.

Der Stream trägt Ereignisnamen **sowohl** im `event:`-Feld als auch in
`data.type` — das schickt OpenAI so, und Clients, die auf `event:` hören, sähen
sonst nichts. Kein `[DONE]`; das Schlussereignis ist `response.completed`.

**Die Summary wird immer angefordert.** `reasoning: {"summary": "auto"}` geht bei
jedem Turn mit, `effort` nur, wenn der Aufrufer einen nennt. Gemessen: das
Backend nimmt `summary` ohne `effort`, die Input-Tokens bleiben identisch, und
die Reasoning-Tokens fallen ohnehin an — das Modell denkt so oder so, die Summary
gießt es nur in Worte. Beides aneinanderzuhängen hieße, dass ein Aufrufer ohne
Effort eine stille Pause sieht und danach eine Antwort aus dem Nichts.

**Denktext wird angehängt, nie überschrieben.** Er kommt als
`response.reasoning_summary_text.delta` — echter Text, an dem es nichts zu
ersetzen gibt. Der Wrapper für Claude muss dort eine Fortschrittszeile *in place*
überschreiben, weil dessen CLI den Denktext redigiert und nur ein Token-Zähler
übrig bleibt. Liefert das Backend die Summary in mehreren Blöcken, wird jeder ein
eigener `summary`-Part (`reasoning_summary_part.added/done`) — sonst klebten die
Absätze aneinander.

**Das fertige Reasoning-Item geht wortwörtlich raus**, `encrypted_content`
inklusive. Es wird nicht aus Feldern nachgebaut: das Backend prüft den Inhalt
kryptografisch (MESSUNGEN.md §9). Wer es unverändert ins nächste `input` legt,
setzt den Turn mit vollem Denkkontext fort — gemessen, siehe unten.

**Bewusst nicht unterstützt: serverseitiger Zustand.** `previous_response_id`
wird mit 400 abgelehnt — stilles Ignorieren hieße, mit halber Unterhaltung zu
antworten, also mit einer falschen Antwort statt einer Fehlermeldung. `store`
wird angenommen und ignoriert (hier wird nichts gespeichert), `background: true`
abgelehnt. Built-in-Tools (`web_search`, `file_search`, …) werden **abgelehnt,
nicht stillschweigend verworfen**: sonst hielte der Client sein Werkzeug für
registriert und wartete auf einen Aufruf, der nie kommt.

Das Schlussereignis bleibt `response.completed`, auch wenn der Status darin
`incomplete` lautet. Die Spezifikation sähe `response.incomplete` vor, aber
Clients werten es nicht aus — Open WebUIs Handler liefert dafür keine Metadaten,
womit `usage` und das Fertig-Signal verloren gehen und die Nachricht nie endet.
Status und `incomplete_details` stehen so oder so im Envelope.

Die `id` (`resp_…`) ist unsere eigene, nicht die `response_id` des Backends: sie
steht schon in `response.created`, bevor die Gegenstelle eine nennt, und eine
mitten im Stream wechselnde `id` wäre schlimmer als eine, die in keinem
OpenAI-Dashboard auftaucht.

### Anmeldung ohne Login-Endpoint

Ist der Dienst nicht arbeitsfähig, schreibt er regelmäßig eine Anmelde-URL ins
Log — beim ersten Start ebenso wie nach einem verlorenen Login:

```
=== ANMELDUNG NOETIG (nicht angemeldet) ===
  1. Öffnen: https://auth.openai.com/codex/device
  2. Code   : BC1N-YMD9Q
  (Code gilt ca. 15 Minuten, danach steht hier ein neuer.)
```

URL im Browser öffnen, Code eintippen — der Dienst pollt selbst und ist danach
bereit. Intervall über `--login-reminder <sekunden>` (Default 300, `0` schaltet
es ab). Solange alles läuft, bleibt das Log still.

**Warum kein Login-Endpoint:** ein solcher wäre mächtiger als jeder
Inferenz-Endpoint — wer ihn erreicht, könnte den Dienst an ein anderes Konto
hängen oder ihn per Dauer-Relogin lahmlegen. Über das Log braucht es dafür keinen
Pfad, keine Rolle und keine zusätzliche Angriffsfläche: wer die Logs lesen darf,
kann anmelden; wer nicht, sieht die URL nie.

### Zugriffsschutz

**Unix-Socket** (Default): die Dateirechte sind die Zugangskontrolle. Der Socket
wird auf `0600` gesetzt; wer ihn öffnen kann, läuft unter derselben uid und
könnte `auth.json` ohnehin lesen. Kein Geheimnis nötig.

**TCP**: API-Schlüssel aus einer Datei, eine Zeile `name:geheimnis`, mehrere
erlaubt und einzeln widerrufbar. Der Name landet im Log — dein Kontingent ist die
geteilte Ressource, und eine Summe über alle Clients sagt nicht, wer sie
verbraucht hat.

**TCP ohne Schlüssel startet nicht.** Kein gewürfelter Default, der wie Sicherheit
aussieht. TLS gehört davor, in den Reverse-Proxy, nicht ins Binary.

### Nutzlast

`POST /wire/v1/responses` nimmt fertige Responses-`input`-Items entgegen — der
Daemon übersetzt nichts. Optional: `instructions`, `tools`, `effort`,
`tool_choice`, `parallel_tool_calls`, `store`, `session_id`.

Jedes SSE-Ereignis ist ein JSON-Objekt in `data:`, unterschieden über `type`:
`started`, `text_delta`, `thinking_delta`, `thinking_break`, `tool_call`,
`reasoning`, `rate_limits`, `done`, `failed`. Kein `event:`-Feld, damit der
Konsument nur an einer Stelle nachsehen muss.

`rate_limits` kommt **mehr als einmal pro Turn**: einmal für das 7-Tage-Kontingent
des Kontos, dann für Zusatzlimits einzelner Modelle. Das Ereignis sagt nicht,
welches es gerade trägt — wer nur das letzte behält, hat das Kontokontingent
verloren. Bis `wire` das unterscheidet, ist Sammeln die einzige verlustfreie
Verarbeitung; `/metrics` macht es vor.

`thinking_break` trägt keinen Text: es markiert die Grenze zwischen zwei
Denkblöcken. Das Backend liefert die Summary in mehreren Teilen, jeder ein
betitelter Absatz — ohne die Grenze liefen sie zu einer Zeile zusammen. Wer keine
Blöcke kennt, ignoriert das Ereignis und verliert nur den Absatzumbruch.

Fehler der Gegenstelle werden **mit Status und Wortlaut durchgereicht** — ein 400
des Backends bleibt ein 400. Nur wo es gar keine Antwort gab (Transport, Auth),
wird daraus 502.

### Prompt-Cache

Der Cache des Backends ist **maschinenlokal**, und der Cache-Key ist die
Wegbeschreibung dorthin. Ohne ihn landet ein Request irgendwo im Pool und trifft
nur zufällig. Gemessen, jeweils warme Turns:

| | Treffer |
|---|---|
| gar kein Key | 22/90 (24 %) — im Betrieb weniger, weil fremde Präfixe dazwischenliegen |
| Key wechselt pro Turn | 0/4 |
| Key stabil über die Unterhaltung | 259/280 (**92 %**) |

Deshalb setzt der Daemon **immer** einen Key — als `session-id`-Header *und* als
Body-Feld `prompt_cache_key`, beide mit demselben Wert, wie der offizielle Client.

Nennt der Aufrufer keinen, wird er aus dem **invarianten Kopf** der Unterhaltung
abgeleitet ([src/client.rs](src/client.rs), `cache_key`): `model`,
`instructions`, `tools` und das **erste Input-Item**. Das erste Item ist der
entscheidende Teil — es trennt zwei Unterhaltungen, die sich Systemprompt und
Tools teilen, und genau das ist der Normalfall bei einem Agenten mit vielen
Unterhaltungen.

Bewusst **nicht** im Hash: alles, was sich zwischen zwei Turns ändern kann, ohne
den Token-Präfix zu brechen — namentlich `effort`. Wer ihn mitten in der
Unterhaltung hochdreht, würde sonst den Cache wegwerfen, obwohl der Präfix
unangetastet ist.

Als Hash **FNV-1a von Hand**, weder Crate noch `DefaultHasher`: der Wert muss über
Neustarts des Daemons identisch bleiben, sonst kostet ein Serviceneustart jeder
laufenden Unterhaltung ihren Cache. `DefaultHasher` sagt darüber nichts zu.

Zwei Grenzfälle, beide bekannt:

- **Gleiche Köpfe kollidieren.** Zwei Unterhaltungen mit byte-identischem Anfang
  bekommen denselben Key. Gemessen folgenlos: drei verschiedene Unterhaltungen
  unter einem Key lagen alle bei 98 %. Der Key routet, der Präfix entscheidet.
- **Ein abgeschnittener Kopf** (Kompaktierung, gleitendes Fenster) verschiebt den
  Key mitten in der Unterhaltung. Dann ist aber auch der Token-Präfix zerstört —
  es gäbe ohnehin nichts zu treffen.

Für den zweiten Fall nehmen beide OpenAI-Oberflächen `prompt_cache_key` aus dem
Request entgegen und lassen ihn gewinnen. Das ist kein Knopf von uns, sondern ein
Feld der OpenAI-Spezifikation mit genau diesem Zweck; ein Client, der es schickt,
meint etwas damit. `user` wird bewusst **nicht** so gedeutet — das Feld benennt
einen Menschen für Missbrauchserkennung, keine Unterhaltung.

### Metriken

`GET /metrics` sagt, was dieser Prozess seit dem Start beobachtet hat. In-Memory,
nichts wird gespeichert: ein Neustart fängt bei null an, und das ist die ehrliche
Lesart — die Zahlen beschreiben den laufenden Prozess, nicht das Konto.

```json
{
  "total_requests": 42, "inflight": 0,
  "outcomes": { "end_turn": 40, "failed": 1, "dropped": 1 },
  "error_rate": 0.0238,
  "cache":   { "hit_rate": 0.978, "read_tokens": 7936, "write_tokens": 0, "input_tokens": 8115 },
  "tokens":  { "input": 8115, "output": 113, "reasoning": 93, "total": 8228 },
  "latency_ms": { "total": { "p50": 2473.2, "p95": …, "p99": …, "n": 2 }, "ttft": { … } },
  "surfaces": { "chat_completions": 40, "responses": 2 },
  "models":  { "gpt-5.6-sol": { "requests": 42, "input_tokens": …, "hit_rate": 0.978 } },
  "rate_limits": [ … ]
}
```

**Die Zahl, auf die es ankommt, ist `cache.hit_rate`.** `cached_input_tokens`
steckt in `input_tokens` (Upstream rechnet `non_cached = input - cached`), das
Verhältnis der beiden ist also direkt mit dem des Claude-Wrappers vergleichbar,
der seines genauso bildet.

Zwei Dinge, die dort bewusst so stehen:

- **`cache.write_tokens` ist immer 0.** Das Feld existiert im Protokoll, das
  Abo-Backend füllt es auf diesem Pfad nicht — gemessen. Es wird trotzdem
  ausgegeben statt versteckt: ein Wert dort wäre eine Neuigkeit.
- **`rate_limits` ist eine Liste, kein Objekt.** Das Backend schickt pro Turn
  *mehrere* Fenster: das 7-Tage-Kontingent des Kontos und getrennt davon
  Zusatzlimits einzelner Modelle. `wire::Event::RateLimits` unterscheidet sie
  nicht, und nur das letzte zu behalten hieße, das Kontokontingent zugunsten
  eines Limits wegzuwerfen, das 0 % anzeigt.

`outcomes` trennt vier Fälle: `end_turn` und `aborted` kommen vom Backend,
`failed` ist ein Fehler im Stream, `rejected` ein Turn, den die Gegenstelle vor
dem ersten Ereignis abgelehnt hat (nach Status aufgeschlüsselt unter
`rejections_by_status`), und `dropped` ein Konsument, der mitten im Stream
aufgelegt hat. In die `error_rate` gehen nur `failed` und `rejected` ein —
auflegen ist kein Fehler dieses Dienstes.

Dazu geht pro Turn eine Zeile ins Log:

```
[local] chat_completions model=gpt-5.6-sol outcome=end_turn total=2399ms ttft=2060ms in=8069 out=31 cached=0/8069 (0.0%)
```

Der Endpunkt braucht **keinen Schlüssel**, aus demselben Grund wie `/health` und
`/ready`: er liegt auf der Wurzel, die das Pfadlayout vom Reverse-Proxy fernhält.
Wer ihn erreicht, ist ohnehin drinnen. Er trägt kein Geheimnis — nur Zähler,
Latenzen und den letzten Kontingentstand.

### Kontingente — ein Format aus zwei Quellen

> Umgesetzt in **1.3.0**. Die Messungen dahinter stehen in
> [MESSUNGEN.md](MESSUNGEN.md) §13 und §15, die Projektion in
> [src/limits.rs](src/limits.rs).

Der Kontingentstand kommt aus **zwei** Quellen, die verschiedene Stärken haben:

- **`GET /wire/v1/usage`** — ein eigener Request an das Backend, liefert immer
  *alle* Gruppen und als einzige die Auskunft „Limit erreicht?".
- **das `rate_limits`-Ereignis jedes Turns** — kostenlos, fällt beim Antworten ab,
  und weiß als einziges, **welche** Gruppe dieser Turn belastet hat.

Beide liefern denselben `limits`-Block, damit ein Konsument nicht wissen muss,
woher ein Objekt stammt.

#### Der `limits`-Block

```json
{
  "active_group": "global",
  "groups": [
    { "id": "global", "name": null, "reached": false,
      "primary":   { "used_percent": 1.0, "window_seconds": 604800,
                     "resets_at": 1788457743, "resets_in_seconds": 591075 },
      "secondary": null },
    { "id": "codex_bengalfox", "name": "GPT-5.3-Codex-Spark", "reached": false,
      "primary":   { "used_percent": 0.0, "window_seconds": 18000,
                     "resets_at": 1787870999, "resets_in_seconds": 4330 },
      "secondary": { "used_percent": 0.0, "window_seconds": 604800,
                     "resets_at": 1788412321, "resets_in_seconds": 545652 } }
  ]
}
```

**Gruppen-Schlüssel.** `"global"` ist **unser** Token für die Gruppe, die das
Backend namenlos lässt; jede andere `id` ist die Backend-Kennung wörtlich
(`metered_feature` in der Usage-API, `limit_id` im Stream). `name` trägt den
Anzeigenamen, sofern das Backend einen nennt — die globale Gruppe hat in keiner
Quelle einen.

`"global"` und nicht `"codex"`, obwohl das die echte Kennung der Default-Familie
ist: im Header-Vokabular bedeutet `codex` „die gerade aktive Familie", bei einem
Spark-Turn also Spark. Diesen Wert zu übernehmen hieße, genau die Mehrdeutigkeit
zu importieren, die dieses Format auflöst.

**`null` heißt überall „unbekannt".** Nur zwei Felder drücken strukturelle
Abwesenheit aus, und die beantworten beide Quellen immer eindeutig: `name: null`
(diese Gruppe hat keinen Namen) und `secondary: null` (diese Gruppe hat kein
zweites Fenster).

**Einheiten sind normalisiert.** `window_seconds` überall — der Stream liefert
Minuten, die Usage-API Sekunden.

#### Der `account`-Block und `promo`

```json
{
  "account": {
    "plan":    { "id": "prolite", "name": "Pro Lite" },
    "credits": { "has_credits": false, "unlimited": false, "balance": "0",
                 "overage_limit_reached": null,
                 "approx_cloud_messages": null, "approx_local_messages": null },
    "spend_control": null,
    "reset_credits": null
  },
  "limits": { "…": "s.o." },
  "promo": null
}
```

`plan.id` ist der Rohwert des Backends, `plan.name` der Anzeigename aus
`KnownPlan::display_name` — bei einem unbekannten Tarif bleibt `name` leer, statt
einen zu erfinden.

Der Plan steht hier **wie vom Backend gemeldet**, nicht aus dem Token. `/wire/v1/whoami`
liefert den aus dem Token, und der ist eine Momentaufnahme vom Ausstellungszeitpunkt:
ändert sich der Tarif ohne Neuanmeldung, hat der Header recht und der Token nicht.
Laufen die beiden auseinander, ist das eine Information, kein Widerspruch.

`credits` beschreibt **Prepaid-Guthaben für Overage**, nicht den Rest des
Kontingents — der ist `100 − used_percent`. Drei der sechs Felder kommen auch aus
dem Stream.

`promo` steht bewusst **außerhalb** von `account`: dort trägt es Werbetext, und
`null` hieße in der Usage-Antwort „keine Kampagne", aus dem Stream aber
„unbekannt". Als eigener Schlüssel, den es nur in der Usage-Antwort gibt, kann es
nicht falsch gelesen werden.

Nicht enthalten sind `account_id`, `user_id` und `email` — die stehen im Token und
damit in [`/wire/v1/whoami`](#anmeldung-ohne-login-endpoint), ohne einen Request.

#### Wer was füllen kann

| | `rate_limits`-Ereignis | `/wire/v1/usage` |
|---|---|---|
| `plan` | ✅ | ✅ |
| `active_group` | ✅ | ❌ sagt nie, welche gilt |
| benannte Gruppen | ✅ immer alle | ✅ |
| Gruppe `global` | ⚠️ **nur wenn sie aktiv ist** | ✅ immer |
| `used_percent`, `window_seconds`, `resets_at`, `resets_in_seconds` | ✅ | ✅ |
| `reached` (an der **Gruppe**, nicht am Fenster — die API meldet es pro Limit) | ❌ | ✅ |
| `credits` | 3 von 6 Feldern | ✅ |
| `spend_control`, `reset_credits`, `promo` | ❌ | ✅ |

Die eine echte Lücke: **aus einer Spark-Antwort ist der globale Stand nicht
ablesbar**, weil die globale Gruppe keine eigene Header-Familie hat und nur
erscheint, wenn sie aktiv ist. Das ist unschädlich — ein Spark-Turn verbraucht
global auch nichts.

#### Was der Wrapper auflöst

Damit ein Konsument nichts davon wissen muss:

1. **Einheiten** normalisieren (Minuten × 60).
2. **Die Default-Familie auflösen.** Benennt `x-codex-active-limit` eine
   vorhandene Gruppe, ist die Default-Familie deren Duplikat und wird verworfen;
   sonst wird sie zur Gruppe `global`. Der Wert selbst (`premium`) wird nie
   interpretiert — gemessen ist er auf Free, Plus und ProLite identisch.
3. **`active_group` setzen**, damit nach jedem Turn feststeht, welche Gruppe er
   belastet hat.
4. **`credits` einmal** an `account`, nicht pro Gruppe — es gibt nur einen Satz
   `x-codex-credits-*`, die Lib hängt ihn aber an jeden Snapshot.

#### Nutzungsmuster

```
einmal beim Start:   GET /wire/v1/usage    volles Bild inklusive `reached`
pro Turn kostenlos:  rate_limits-Ereignis  aktualisiert die Gruppe aus `active_group`
bei Bedarf:          GET /wire/v1/usage    Abgleich; das Einzige, was `reached` beantwortet
```

Die Gruppen-Schlüssel sind in beiden Quellen dieselben, der Abgleich ist also eine
Map `id → Gruppe`.

#### Woher die Header kommen

`codex-api` verbraucht die Antwort-Header intern und reicht nur weiter, was seine
eigenen Typen abbilden — ohne `x-codex-active-limit` und `x-codex-plan-type`, und
mit fest verdrahtetem `plan_type: None`. Der Daemon legt deshalb einen Dekorator
um den Transport (`HttpTransport` ist öffentlich, `ResponsesClient` generisch
darüber) und liest die Header selbst, bevor die Bibliothek sie verbraucht. Kein
Fork, kein nachgebauter SSE-Decoder; ändert sich das Trait, bricht der Build.

Fehlt `x-codex-active-limit` einmal, bleibt die Default-Familie unaufgelöst: sie
behält die Backend-Kennung `codex` und `active_group` bleibt `null`. Unbekannt
statt geraten.

Praktischer Hinweis, der so bleibt: `used_percent` ist **ganzzahlig**, auch als
Fließkommazahl (`46.0`). Unter einem Prozentpunkt ist nichts zu sehen, und aus
einem unveränderten Wert folgt nicht, dass nichts verbraucht wurde. Wer feiner
messen will, nimmt die Tokenzahlen aus `/metrics`.

### Warum HTTP und nicht stdio

Multiplexing und Cancellation gibt es hier geschenkt: eine Verbindung pro
Request, Abbruch heißt Verbindung fallenlassen. Über ein Zeilenprotokoll wären
beide von Hand zu bauen. Und SSE ist ohnehin das Format, das oben hereinkommt.

Der Einwand gegen HTTP war der Zugriffsschutz — eine Pipe hat ihn eingebaut, ein
Port nicht. Der Unix-Socket löst genau das: HTTP-Semantik mit Pipe-artiger
Zugangskontrolle.

## Messungen reproduzieren

```bash
python3 scripts/measure.py            # braucht httpx und einen Login
```

Fährt die Fälle aus [MESSUNGEN.md](MESSUNGEN.md) durch, startet den Daemon selbst
und räumt auf. Verbraucht echtes Abo-Kontingent, die Prompts sind entsprechend
knapp.

## Container

`docker build -t codex-api-wrapper .` — zweistufig, klont `openai/codex` auf den
Commit aus [CODEX_REV](CODEX_REV).

Die interessante Frage dabei ist die Anmeldung, nicht der Build: ein OAuth-Flow
setzt einen Menschen mit Browser voraus, und `codex-login` schreibt beim Refresh
**rotierte Tokens zurück** — ein read-only Secret bricht deshalb. Ausführlich in
**[DEPLOY.md](DEPLOY.md)**.

### Login auf einer Remote-Maschine (Browser-Flow)

Einfacher ist `login --device`. Wer den Browser-Flow will:
der Callback-Server bindet **fest Port 1455** — die `redirect_uri` ist bei OpenAI
registriert und nicht wählbar. Läuft der PoC auf einem Remote-Host, muss der Port
zum Browser-Rechner weitergeleitet werden:

```bash
ssh -L 1455:127.0.0.1:1455 remote-host
```

`CODEX_WRAPPER_NO_BROWSER=1` unterdrückt den Versuch, lokal einen Browser zu
öffnen. Die Login-URL wird immer ausgegeben.

## Messergebnisse

**Die zentralen Fragen sind beantwortet — siehe [MESSUNGEN.md](MESSUNGEN.md).**
Kurzfassung, gemessen am 2026-08-24 gegen einen Plus-Account:

| Frage | Erwartung laut §8.3/§8.4 | Gemessen |
|---|---|---|
| Nimmt das Backend fehlende/eigene `instructions`? | nein, `400` | **ja, 200 OK** — auch ganz ohne, 12–15 Input-Tokens |
| Kommen eigene Tools durch? | nur mit gepatchtem Fork | **ja, direkt** — nativer `function_call`, keine Built-ins |
| Schließt der Tool-Round-Trip? | offen | **ja** — `function_call_output` als natives Item |
| Mehrere Tool-Calls pro Turn? | nein (CLI-Decke bei Claude) | **ja**, mit `parallel_tool_calls` |
| Echte Reasoning-Summaries? | ja | **ja**, Klartext |
| Rate-Limit-Daten? | offen | **im Response-Header**, `x-codex-*` |
| `apply_patch` als `custom` oder `function`? | offen (§10) | `freeform` — für uns folgenlos |

Damit ist der Provider-Vertrag aus §11.1 hier **ohne jede Konstruktion** erfüllt:
kein MCP-Stall, kein Interrupt, kein `--tools ""`, kein Prozess-Pool, kein Fork.
Die vollständige Agent-Schleife läuft ohne `store` und ohne
`previous_response_id` — der Verlauf reist im `input` mit.

## Wofür das Werkzeug gebaut ist

Die Kommandos, mit denen die Messungen entstanden sind — reproduzierbar.

### 1. Nimmt das Backend leere `instructions`?

Der Default sendet **kein** `instructions`-Feld — der erste Aufruf ist also schon
der Test.

```bash
codex-api-wrapper ask "Antworte nur mit dem Wort: OK" \
  --raw --dump-dir dumps/no-instructions
codex-api-wrapper ask "Wer bist du? Ein Satz." --raw --dump-dir dumps/eigene-instructions \
  --instructions "Du bist WYAI. Du bist NICHT Codex. Beginne jede Antwort mit 'WYAI:'."
```

Beide **200 OK**. Der erste Lauf verbraucht 15 Input-Tokens — Beleg, dass kein
Systemprompt serverseitig dazukommt. Der zweite übernimmt die fremde Persona
vollständig. Damit entfällt der in §8.3 beschriebene Nachteil gegenüber Claude
(dort **ersetzt** `--system-prompt` den Default) ersatzlos.

### 2. Kommen eigene Tools durch?

Der Angelpunkt: beim Claude-Wrapper macht `--tools ""` die CLI zur reinen
Inferenzschicht (§1.1).

```bash
codex-api-wrapper ask "Wie ist das Wetter in Berlin? Nutze das Tool." \
  --tools-file examples/tools-minimal.json --raw --dump-dir dumps/own-tools
```

**Ja** — ein nativer `function_call` auf unser eigenes Tool, keine Built-ins im
Envelope. Issue `openai/codex#6049` betrifft die CLI, nicht den Endpoint. Der
Default sendet `"tools": []` (Simon Willisons Messung, direkt nachvollziehbar).

### 3. Was liefert `/models`?

```bash
codex-api-wrapper models
```

Modell-Liste mit Effort-Stufen, `service_tiers`, `token_budget` — und pro Modell
ein `model_messages.instructions_template`: **das Backend gibt den Systemprompt
selbst aus**, der Client setzt ihn ein. Material für die `/v1/models`-Lücke aus
dem Feature-Audit (§6).

### 4. Rate-Limits und Reasoning-Summaries

`ResponseEvent::RateLimits` kommt im dekodierten Modus als eigene Zeile, im
Raw-Modus als `x-codex-*`-Header — die Daten für den `GET /v1/key`-Punkt aus §7.

```bash
codex-api-wrapper ask "Ein Rätsel, das Nachdenken erzwingt ..." --effort high --raw
```

`effort` wird angenommen, der Server hebt `summary` auf `detailed` an, und es
kommen echte `reasoning_summary_text`-Events in Klartext — anders als Claudes
redigierter Denktext.

## Die zwei Ausführungsmodi

| Modus | Weg | Wofür |
|---|---|---|
| Default | `codex_api::ResponsesClient` | derselbe Code wie die echte CLI: SSE-Parsing, Idle-Timeout, typisierte Events |
| `--raw` | direkt am Transport | ungeparster SSE-Text, alle Response-Header, Status |

`--raw` ist der wichtigere für Protokollfragen: Envelope-Felder, die der
dekodierte Pfad wegabstrahiert (`instructions`, `tools`, `prompt_cache_key`,
`reasoning`), alle Response-Header und der Wortlaut eines Fehlers.

`--dump-dir` schreibt in beiden Modi `request.json`; im Raw-Modus zusätzlich
`response-headers.txt` und `response-raw.sse`, im dekodierten `events.log`.

## Bewusste Entscheidungen

- **Retries aus** (`max_attempts: 1`). Ein Erkundungswerkzeug soll den ersten
  Fehler zeigen. 429 und 5xx sind hier das Messergebnis, nicht die Störung.
- **Kein API-Key-Fallback** (`enable_codex_api_key_env: false`). Ein herumliegendes
  `OPENAI_API_KEY` würde still gegen die Platform-API messen statt gegen das Abo.
  Der PoC soll ausschließlich den Abo-Pfad vermessen; ein Ergebnis von der falschen
  Gegenstelle wäre schlimmer als keines.
- **Credentials als Datei**, nicht im OS-Keyring. Inspizierbar, und ein gelöschter
  Ordner lässt nichts zurück.
- **Token bei jedem Request neu geholt** über `AuthManager::auth()`, statt einmal
  als Snapshot. Erneuert ein abgelaufenes Access-Token unterwegs.
- **Request-Body als freies JSON**, nicht über `ResponsesApiRequest`. Für ein
  Erkundungswerkzeug ist beliebige Variierbarkeit von `instructions` und `tools`
  der Punkt.
- **Metriken in-memory, kein Prometheus-Format.** Ein einzelner Prozess ohne
  Persistenz braucht keinen Scraper; JSON liest sich mit `curl` und `jq` direkt.
  Wer Prometheus will, setzt einen Exporter davor — dann liegt die Entscheidung
  bei ihm und nicht im Binary.

## Bekannte Lücken

- **Kein Prozess-Neustart.** Stirbt der Daemon, merken das nur laufende Requests;
  er startet sich nicht selbst neu. Das gehört zum Aufrufer.
- `session-id` wird selbst erzeugt statt über das `uuid`-Crate. Die Sorge dahinter
  ist ausgeräumt: gemessen nimmt das Backend **beliebige Zeichenketten** an, eine
  UUID-Form wird nicht geprüft. Der Wert muss nur über die Turns einer
  Unterhaltung stabil bleiben — daran hängt der Cache, nicht an seiner Form.
- **`wire` unterscheidet Konto- und Modell-Limit nicht.** `Event::RateLimits`
  trägt kein Merkmal, aus dem hervorginge, welches Fenster es beschreibt. Solange
  das so ist, muss jeder Konsument alle sammeln.
- **Metriken überleben keinen Neustart** und aggregieren nicht über Prozesse. Für
  einen Dienst, der als ein Prozess läuft, ist das gewollt — als Kontingentbuch
  taugt es nicht, dafür ist `/wire/v1/usage` da.
- **Tests nur für die Übersetzungsschichten und die Metriken.** `cargo test` deckt
  `openai`, `openai_chat`, `openai_responses` und `metrics` ab — reine Abbildung
  bzw. Buchführung, ohne Gegenstelle. Alles darunter (Auth, Transport, `client`)
  bräuchte ein Mock-Backend, das es noch nicht gibt. `scripts/measure.py` prüft
  nichts, es *zeigt* — Bewertung macht der Mensch.

## Lizenz / Attribution

Bindet `openai/codex` (Apache-2.0) per Pfad ein. Apache §6 verbietet Markennutzung
— bei einer Veröffentlichung darf „Codex" nicht im Produktnamen stehen und
NOTICE/Attribution müssen mit. Für einen lokalen PoC nicht relevant, für alles
danach schon.
