# Product direction

The core is a shelf of resumable work: find a session, open it, put it away, and restore it. Named groups should make those actions work on a project or collection. Resource use helps decide what to put away; it is a secondary view.

## Home view

- Short human titles, project and one clear next action.
- Search names, directories, identities and cached descriptions.
- Selected work shows purpose, last progress, next step and blockers.
- Technical IDs, hooks and process trees appear on request.
- Usage shows RAM and E/P CPU/GPU history, with pressure and swap context.

## Next workflow

Give each item one home group. Add existing items selectively; saving a Ghostty inventory must not silently redefine membership. Show groups with item counts and expand them into sessions.

Group Open focuses existing work and restores saved conversations in their recorded folders. Group Put away saves exact bindings, asks for provider completion/exit where necessary, and closes eligible surfaces. Results must distinguish work actually put away from active items needing attention. Restore uses the recorded successful batch, with a review if state has changed.

The same workflow should serve people and agents. Human commands can wrap durable preview/apply receipts so routine use does not require copying UUIDs. Agent commands keep stable identities, explicit blockers, exact targets and controller protection. A description or an idle CPU sample cannot authorize stopping work.

## Current limits

The readable session view, search, exact resume bindings and secondary usage plots are implemented. Selective group membership and first-class group close are still planned. `save GROUP` currently imports the full Ghostty inventory. `gws shutdown` is not implemented; provider shutdown remains manual-only, followed by the existing reviewed parking/restore operations. Physical reboot continuation still needs real-provider acceptance testing.

Sol reviewed the view and Astra reviewed the broader workflow. Their shared recommendation is to make finding and managing work primary, with diagnostics available when needed.
