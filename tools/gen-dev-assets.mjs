// Génère ui/dev-assets.json (même forme que la commande Rust get_assets) pour la prévisualisation navigateur.
const L = "language=fr-FR";
const j = (u) => fetch(u).then((r) => r.json()).then((r) => r.data);
const pick = (o, ks) => Object.fromEntries(ks.map((k) => [k, o[k]]));
const [agents, tiers, maps, cards, borders] = await Promise.all([
  j(`https://valorant-api.com/v1/agents?isPlayableCharacter=true&${L}`),
  j(`https://valorant-api.com/v1/competitivetiers?${L}`),
  j(`https://valorant-api.com/v1/maps?${L}`),
  j(`https://valorant-api.com/v1/playercards`),
  j(`https://valorant-api.com/v1/levelborders`),
]);
const out = {
  agents: agents.map((a) => ({ ...pick(a, ["uuid", "displayName", "displayIcon", "fullPortrait", "backgroundGradientColors"]), role: a.role ? { displayName: a.role.displayName, displayIcon: a.role.displayIcon } : null })),
  tiers: tiers.at(-1).tiers.map((t) => pick(t, ["tier", "tierName", "color", "backgroundColor", "largeIcon", "smallIcon"])),
  maps: maps.map((m) => pick(m, ["uuid", "displayName", "mapUrl", "splash", "listViewIcon"])),
  levelBorders: borders.map((b) => pick(b, ["startingLevel", "levelNumberAppearance"])),
  cards: cards.filter((c) => c.wideArt && c.largeArt).filter((_, i) => i % 37 === 0).slice(0, 24).map((c) => c.uuid),
};
(await import("fs")).writeFileSync(process.argv[2], JSON.stringify(out));
console.log(out.agents.length, out.tiers.length, out.maps.length, out.cards.length, out.tiers.slice(-4).map(t=>t.tierName));
