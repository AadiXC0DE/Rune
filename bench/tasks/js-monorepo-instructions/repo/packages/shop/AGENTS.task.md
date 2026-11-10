# shop

Never edit files in `generated/`. They are produced from `catalog.json`; after changing
`catalog.json`, regenerate them with `node scripts/gen.js` (run from this directory).

Prices are whole cents. The shop shows a price per unit and a total, and the total
must equal the unit price times the quantity, summed over the lines.
