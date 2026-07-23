# TUI-Ausbauplan

## Ziel

Die bestehende TUI soll sich vom Snapshot- und Dateibrowser zu einem sicheren
Backup-Dashboard entwickeln. Der Ausbau priorisiert lesende Funktionen und den
normalen Restore in ein separates Ziel. Destruktive Repository-Aktionen bleiben
bewusst ausserhalb der TUI.

## Aktueller Stand

Die TUI bietet derzeit:

- Snapshot-Browsing aus dem lokalen SQLite-Index;
- Navigation durch direkte Verzeichniseintraege;
- Filterung des aktuell sichtbaren Verzeichnisses;
- Auswahl mehrerer Pfade;
- Originalpfad-Restore mit expliziter Bestaetigung und Fortschrittsanzeige;
- CLI-Hinweise fuer Mount und Snapshot-Diff.

Der Index ist nur ein Browse-Cache. Restore, Repository-Pruefungen und andere
sicherheitsrelevante Entscheidungen muessen weiterhin mit Live-Borg-Daten
validiert werden.

## Leitlinien

- Sichere und lesende Funktionen zuerst umsetzen.
- Cache- und Live-Daten jederzeit deutlich unterscheiden.
- Den normalen Restore in ein separates Ziel als Standard anbieten.
- Originalpfad-Restore als fortgeschrittene, destruktive Aktion kennzeichnen.
- Sicherheitspruefungen im Backend und Root-Helper nicht in die UI verlagern.
- Keine Secrets in Argumenten, UI-Ausgaben, Logs oder Fehlermeldungen anzeigen.
- Lange Aktionen asynchron ausfuehren und ihren Fortschritt sichtbar machen.
- Desktop- und kleine Terminalgroessen unterstuetzen.

## Phase 1: Browsing und Auswahl

### 1. Globale Suche

- Den bestehenden Index fuer eine Suche im gesamten Snapshot verwenden.
- Optional ueber alle Snapshots suchen.
- Treffer mit Snapshot, Pfad, Typ, Groesse und Zeit anzeigen.
- Aus einem Treffer in das zugehoerige Verzeichnis springen.
- Suchergebnisse eindeutig als Cache-Daten kennzeichnen.

Abnahmekriterien:

- Die Suche ist nicht auf das aktuell sichtbare Verzeichnis beschraenkt.
- Ein Treffer kann im Browser geoeffnet und ausgewaehlt werden.
- Leere Ergebnisse und unvollstaendige oder veraltete Indizes werden erklaert.

### 2. Selection Manager

- Overlay mit allen ausgewaehlten Pfaden bereitstellen.
- Einzelne Eintraege entfernen und die gesamte Auswahl leeren koennen.
- Anzahl der ausgewaehlten Pfade dauerhaft sichtbar machen.
- Eltern-/Kind-Auswahlen vor einem Restore nachvollziehbar zusammenfassen.
- Ausgewaehlte Pfade aus anderen Verzeichnissen sichtbar halten.

Abnahmekriterien:

- Keine aktive Auswahl bleibt fuer den Benutzer unsichtbar.
- Snapshot-Wechsel und Auswahl-Loeschung verhalten sich eindeutig.
- Restore-Dialog und Selection Manager zeigen dieselben effektiven Pfade.

### 3. Erweiterte Detailansicht

- Modus und Dateirechte, UID/GID und Symlink-Ziel anzeigen.
- Vollstaendigen Pfad, Snapshot-Zeit und Borg-Archiv-ID anzeigen.
- Lange Werte, Fehler und Metadaten scrollbar darstellen.
- Dateitypen und besondere Eintraege klar kennzeichnen.

## Phase 2: Status und Cache-Transparenz

### 4. Backup-Dashboard

- Host- und Profil-ID anzeigen.
- Letztes erfolgreiches Backup und naechste Faelligkeit anzeigen.
- Repository-, Index- und Lock-Status anzeigen.
- Letzte Jobs mit Ergebnis, Laufzeit und sicher bereinigter Fehlermeldung zeigen.
- Sichtbar zwischen `cached`, `live`, `stale` und `incomplete` unterscheiden.

Abnahmekriterien:

- Der Benutzer kann erkennen, wie aktuell die angezeigten Daten sind.
- Ein fehlerhafter letzter Job ist ohne CLI-Wechsel auffindbar.
- Repository-Orte und Fehlermeldungen geben keine Secrets preis.

### 5. Index Refresh und Live-Abgleich

- Index-Refresh explizit aus der TUI starten.
- Fortschritt, Erfolg und Fehler anzeigen.
- Nach dem Refresh Snapshots und aktuelle Ansicht kontrolliert neu laden.
- Optional einen Live-Abgleich der Snapshot-Liste anbieten.
- Cache-Daten niemals stillschweigend als Live-Ergebnis darstellen.

Abnahmekriterien:

- Die TUI bleibt waehrend des Refresh responsiv.
- Abbruch oder Fehler hinterlassen keinen als vollstaendig markierten Teilindex.
- Ein Live-/Cache-Unterschied wird deutlich dargestellt.

## Phase 3: Restore-Workflow

### 6. Sicherer Staging-Restore

- Restore in ein neues oder leeres absolutes Zielverzeichnis anbieten.
- Zielpfad in einem eigenen Dialog erfassen und validieren.
- Normalen Restore als Standardaktion verwenden.
- Originalpfad-Restore getrennt als fortgeschrittene Aktion darstellen.
- Bestehende Backend-Pruefungen und atomare Publikation unveraendert nutzen.

Abnahmekriterien:

- Ein normales Ziel darf weder nichtleer noch ein Symlink sein.
- Bestehende Eltern werden auf Symlink-Traversal geprueft.
- Der Restore basiert auf Live-Manifest und unveraenderlicher Borg-Archiv-ID.
- Der Dialog erklaert klar, ob Daten separat publiziert oder ersetzt werden.

### 7. Live-Preflight und Restore-Review

- Vor der finalen Bestaetigung die Live-Validierung starten.
- Borg-Archiv-ID, Datei- und Byteanzahl anzeigen.
- Effektive Restore-Wurzeln und minimierte Eltern-/Kind-Auswahlen anzeigen.
- Staging-Dateisystem und Publikationsziel darstellen.
- Geschuetzte Pfade, Limits und spezielle Dateien vor der Bestaetigung melden.
- Alle Ziele scrollbar anzeigen, statt nur eine kleine Vorschau zu bieten.

Abnahmekriterien:

- Die finale Bestaetigung bezieht sich auf den validierten Live-Plan.
- Veraendert sich die Archiv-ID, wird der Vorgang abgebrochen.
- Mehrpfad-Restores weisen auf fehlende Gesamttransaktionalitaet hin.
- Originalpfad-Restore verlangt weiterhin exakt `RESTORE`.

## Phase 4: Vergleich und Mounts

### 8. Echter Snapshot-Diff

- Zwei Snapshots in der TUI markieren.
- Neue, geaenderte und geloeschte Pfade getrennt darstellen.
- Den Diff optional auf den aktuellen Pfad begrenzen.
- Ergebnisse streamen und grosse Ausgaben scrollbar machen.
- Filter fuer Aenderungstypen anbieten.

Abnahmekriterien:

- `D` zeigt nicht mehr nur einen CLI-Hinweis.
- Die Auswahl der beiden Snapshots ist jederzeit sichtbar.
- Ein Diff veraendert weder Repository noch lokalen Index.

### 9. Mount Manager

- Mount-Ziel erfassen und als leer, lokal und symlinkfrei validieren.
- Aktive Boxup-Mounts anzeigen.
- Unmount aus der TUI anbieten.
- Fehler bei fehlendem FUSE oder Borg nachvollziehbar darstellen.
- Einen Mount niemals als erfolgreichen Restore-Test bezeichnen.

Abnahmekriterien:

- Mount und Unmount verwenden weiterhin shellfreie Prozesse.
- Die TUI zeigt den tatsaechlichen Mount-Zustand.
- Fremde oder nicht eindeutig zuordenbare Mounts werden nicht veraendert.

## Phase 5: Bedienung und Betrieb

### 10. Hilfe und Command Palette

- `?` oeffnet eine kontextabhaengige Shortcut-Uebersicht.
- Eine Command Palette listet Aktionen des aktuellen Screens.
- Gefaehrliche Aktionen werden nicht direkt neben haeufigen Leseaktionen
  platziert.
- Kleine Terminals erhalten kompakte Layouts statt abgeschnittener Dialoge.

### 11. Manueller Backup-Job

- Einen normalen Backup-Lauf aus der TUI starten.
- Aktuelle Phase, Dauer und sichere Fortschrittsdaten anzeigen.
- Parallele Backup-Laeufe durch die bestehende Sperre verhindern.
- Nach Erfolg Status und Index kontrolliert aktualisieren.

Diese Funktion folgt erst nach den lesenden und Restore-bezogenen Funktionen,
weil sie Remote-Schreibzugriff ausloest und mehr Betriebszustaende abbilden muss.

## Bewusst nicht in der TUI

Die folgenden Aktionen bleiben CLI-only:

- Repository-Initialisierung;
- Prune und Compact;
- Export oder Verwaltung von Schluesselmaterial;
- Emergency-Root-Overwrite nach `/`;
- Aenderung von Profilen, Credentials oder Timer-Aktivierung.

Diese Funktionen sind selten, folgenreich oder benoetigen eine bewusste
administrative Prozedur. Eine leicht erreichbare TUI-Aktion wuerde hier keinen
Sicherheitsgewinn bieten.

## Technische Richtung

- Die bisherige einzelne TUI-Zustandsstruktur schrittweise in Screens und
  modale Dialoge aufteilen, sobald die ersten neuen Ansichten hinzukommen.
- Datenzugriff, Prozessausfuehrung und Rendering getrennt halten.
- Worker-Nachrichten typisieren, statt Fortschritt nur als freie Textzeilen zu
  behandeln.
- Lange Borg-Aktionen in Hintergrund-Workern ausfuehren.
- Abbruch nur in eindeutig sicheren Phasen erlauben; Publikation nicht mitten in
  einer atomaren Ersetzung unterbrechen.
- Fehlerausgaben zentral bereinigen und scrollbar speichern.
- Bestehende shellfreie Borg-, SSH-, pkexec- und Mount-Ausfuehrung beibehalten.

## Tests

Fuer jede Phase sind mindestens folgende Tests vorzusehen:

- Zustands- und Keybinding-Tests fuer alle neuen Modi;
- Render-Tests fuer normale und kleine Terminals;
- Navigation zwischen Suche, Browser und Auswahl;
- Cache-/Live-Kennzeichnung und veraltete Indizes;
- Worker-Erfolg, Fehler, ungueltige Nachrichten und Terminal-Cleanup;
- Argumenttests fuer gestartete Prozesse ohne Secrets;
- Restore-Preflight, Archiv-ID-Wechsel und geschuetzte Pfade;
- nebenlaeufiger Index-Refresh und kontrolliertes Reload;
- Mount-/Unmount-Zustand mit isolierten Test-Doubles.

Reale Borg-Tests duerfen nur Borg 1.4, ein frisches temporaeres Repository,
keinen Netzwerkzugriff und keine Root-Rechte verwenden.

## Empfohlene Umsetzungsreihenfolge

1. Globale Suche und Treffer-Navigation.
2. Selection Manager und erweiterte Detailansicht.
3. Backup-Dashboard mit eindeutiger Cache-Provenienz.
4. Sicherer Staging-Restore.
5. Live-Preflight und verbesserter Restore-Review.
6. Echter Snapshot-Diff.
7. Index-Refresh und Live-Abgleich.
8. Mount Manager.
9. Hilfe, Command Palette und responsive Layouts.
10. Manueller Backup-Job.

Der erste Lieferblock sollte globale Suche, Treffer-Navigation und Selection
Manager enthalten. Er bietet hohen Alltagsnutzen, verwendet bereits vorhandene
Index-Funktionen und erweitert noch keine destruktiven Oberflaechen.
