Patches against mozilla-central to enable prototype remoting for cubeb.

Once the patches have been applied, rust crates need to be updated:
* execute ``cargo update -p gkrust-shared`` in ``toolkit/library/rust`` to update Cargo.lock.
* execute ``./mach vendor rust`` to update ``third_party/`` dir.
* enable feature in mozconfig with ``ac_add_options --enable-cubeb-remoting``
* enable ``media.cubeb.sandbox`` via about:config/prefs.js
