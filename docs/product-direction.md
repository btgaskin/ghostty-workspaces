# Product direction

The core is a shelf of resumable work: find a session, open it, put it away, and restore it. Named groups should make those actions work on a project or collection. A compact usage column keeps the whole machine visible alongside the work.

## Home view

- Short human titles, separate location/provider columns and tiny attention markers.
- Search names, directories, identities and cached descriptions.
- Selected work shows purpose, last progress, next step and blockers.
- Technical IDs, hooks and process trees appear on request.
- Usage runs down the right side with RAM and E/P CPU/GPU history, pressure and swap context.
- Selected activity separates observed subagents from tracked processes; binding has a guided shortcut.

## Next workflow

Give each item one home group. Add existing items selectively; saving a Ghostty inventory must not silently redefine membership. Show groups with item counts and expand them into sessions.

Group Open focuses existing work and restores saved conversations in their recorded folders. Group Put away saves exact bindings, asks for provider completion/exit where necessary, and closes eligible surfaces. Results must distinguish work actually put away from active items needing attention. Restore uses the recorded successful batch, with a review if state has changed.

The same workflow should serve people and agents. Human commands can wrap durable preview/apply receipts so routine use does not require copying UUIDs. Agent commands keep stable identities, explicit blockers, exact targets and controller protection. A description or an idle CPU sample cannot authorize stopping work.

## Current limits

The readable session overview, search, exact resume bindings, read/unread completion markers and visible usage plots are implemented. Selective group membership and first-class group close are still planned. `save GROUP` currently imports the full Ghostty inventory. `gws shutdown` is not implemented; provider shutdown remains manual-only, followed by the existing reviewed parking/restore operations. Physical reboot continuation still needs real-provider acceptance testing.

Sol reviewed the view and Astra reviewed the broader workflow. Their shared recommendation is to make finding and managing work primary, with diagnostics available when needed.
