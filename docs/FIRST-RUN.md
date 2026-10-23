# First-run setup

Kwil shows a short welcome explaining its editing benefits first, then the implementation behind them. Release builds remember completed setup in the selected settings profile. Debug builds show setup on every launch and never read or write that completion marker.

The next screen offers 240 common text extensions, grouped by purpose. Choose Common, All text types, Plain text, or Custom; individual choices and additional extension text are editable. Selecting a preset does not change the operating system. Review changes, then choose Register selected, or Skip for now.

On Windows this adds per-user Open with entries. It does not replace protected default-app choices; use Windows Default apps to change those. Registration is one bounded, journaled transaction, with an explicit Unregister operation that restores previous values when they have not been changed by another application. Existing conflicts or incomplete verification are reported without claiming that Kwil is the current default.

Settings uses the same File types page. Preparing native registration and writing completion state happen on workers. Setup owns keyboard, pointer and IME input until it completes, while accepted external opens remain bounded and are delivered afterward.

## Current boundaries

- Native first-run registration is implemented for Windows. macOS and Linux report unavailable until their application-bundle/MIME installation contexts are connected.
- Adding new extensions to an already retained registration and history rotation remain separate work. The UI reports this limitation rather than silently unregistering and creating a new transaction.
- The native Open/Save As dialog and Find/Go to host packages are separate from this change.
- Tests and hidden captures use explicit fake association adapters and isolated storage. They never change the owner's associations. Native platform and owner acceptance are tracked in Pair Desk KW-46 and KW-55.