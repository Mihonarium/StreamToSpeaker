// Fit the mark's geometry to reference.png. Produces the numbers in
// generate.py; run it instead of nudging those numbers by hand.
//
//   node assets/brand/fit.mjs           # search, print the winning parameters
//   node assets/brand/fit.mjs --score   # just score what generate.py ships
//
// Set CHROME to a Chromium/Chrome binary. Scoring happens against real SVG
// rasterisation rather than a stand-in renderer: an earlier version scored
// candidates with a 2D drawing library, and the parameters that won there
// dropped from 0.79 to 0.38 once actually rendered as SVG — different
// libraries place a stroke differently relative to its path.
import {chromium} from 'playwright-core';
import {readFileSync} from 'fs';
import {fileURLToPath} from 'url';
import {dirname, join} from 'path';

const HERE = dirname(fileURLToPath(import.meta.url));
const SCORE_ONLY = process.argv.includes('--score');
const refB64 = readFileSync(join(HERE, 'reference.png')).toString('base64');

// Starting point and search bounds. Bounds only keep the search sane; the
// optimum sits well inside them.
const START = {sx: 43, sy: 133, sw: 94, sh: 66, srad: 3, stroke: 11,
               bx: 43, by: 199, btw: 94, bh: 26, bflare: 12,
               cx: 97, cy: 160, r0: 71, rgap: 30, astroke: 15, a0: 9, a1: 82};

const br = await chromium.launch({
    executablePath: process.env.CHROME, args: ['--no-sandbox'],
});
const pg = await br.newPage({viewport: {width: 300, height: 300}});
await pg.setContent('<canvas id="a" width="256" height="256"></canvas>');

const out = await pg.evaluate(async ({refB64, START, SCORE_ONLY}) => {
    const S = 256;
    const ctx = document.getElementById('a').getContext('2d', {willReadFrequently: true});
    const load = src => new Promise((res, rej) => {
        const im = new Image(); im.onload = () => res(im); im.onerror = rej; im.src = src;
    });

    // 1 = white ink, 2 = coral ink, 0 = neither. Thresholds are wide enough
    // to survive the reference being a lossy raster.
    function maskOf(img) {
        ctx.clearRect(0, 0, S, S);
        ctx.drawImage(img, 0, 0, S, S);
        const d = ctx.getImageData(0, 0, S, S).data;
        const m = new Uint8Array(S * S);
        for (let i = 0, p = 0; i < d.length; i += 4, p++) {
            const [r, g, b, a] = [d[i], d[i + 1], d[i + 2], d[i + 3]];
            if (a < 128) continue;
            if (Math.min(r, g, b) > 175) m[p] = 1;
            else if (r > 170 && g < 150 && b < 130) m[p] = 2;
        }
        return m;
    }
    const ref = maskOf(await load('data:image/png;base64,' + refB64));

    const arcPath = (cx, cy, r, a0, a1) => {
        const rad = d => d * Math.PI / 180;
        const p = (a) => [cx + r * Math.cos(rad(a)), cy - r * Math.sin(rad(a))];
        const [x0, y0] = p(a0), [x1, y1] = p(a1);
        return `M ${x0.toFixed(1)} ${y0.toFixed(1)} A ${r} ${r} 0 0 1 ${x1.toFixed(1)} ${y1.toFixed(1)}`;
    };
    // Must stay in step with generate.py — same shapes, same order. The
    // colours here are the REFERENCE's (white on an indigo tile), not the
    // ones the icons ship in: this fits geometry, and the masks only need to
    // separate laptop from arcs in both images.
    const svgOf = p => `<svg xmlns="http://www.w3.org/2000/svg" width="${S}" height="${S}"
        viewBox="0 0 ${S} ${S}"><rect width="${S}" height="${S}" fill="#303086"/>
        <g fill="none" stroke="#ffffff" stroke-width="${p.stroke}"
           stroke-linejoin="round" stroke-linecap="round">
          <rect x="${p.sx}" y="${p.sy}" width="${p.sw}" height="${p.sh}" rx="${p.srad}"/>
          <path d="M ${p.bx} ${p.by} L ${p.bx + p.btw} ${p.by}
                   L ${p.bx + p.btw + p.bflare} ${p.by + p.bh}
                   L ${p.bx - p.bflare} ${p.by + p.bh} Z"/></g>
        ${[0, 1, 2].map(i => `<path d="${arcPath(p.cx, p.cy, p.r0 + i * p.rgap, p.a1, p.a0)}"
           fill="none" stroke="#fe6045" stroke-width="${p.astroke}"
           stroke-linecap="round"/>`).join('')}</svg>`;

    async function score(p) {
        const m = maskOf(await load('data:image/svg+xml;base64,' + btoa(svgOf(p))));
        let iw = 0, uw = 0, ic = 0, uc = 0;
        for (let i = 0; i < m.length; i++) {
            const aw = m[i] === 1, bw = ref[i] === 1;
            if (aw || bw) { uw++; if (aw && bw) iw++; }
            const ac = m[i] === 2, bc = ref[i] === 2;
            if (ac || bc) { uc++; if (ac && bc) ic++; }
        }
        // The arcs carry more of the mark's identity than the laptop does.
        return {w: iw / uw, c: ic / uc, s: 0.45 * (iw / uw) + 0.55 * (ic / uc)};
    }

    let p = {...START};
    if (SCORE_ONLY) return {p, final: await score(p), trace: []};

    const bounds = {stroke: [3, 16], astroke: [6, 28], srad: [0, 26], r0: [40, 115],
                    rgap: [16, 48], a0: [-12, 32], a1: [58, 112], bh: [6, 44],
                    bflare: [0, 32], sw: [50, 150], sh: [30, 110], btw: [50, 150]};
    let step = Object.fromEntries(Object.keys(p).map(k => [k, 4]));
    let best = (await score(p)).s;
    const trace = [`start ${best.toFixed(4)}`];
    // Coordinate descent, halving the step each time it stalls, until every
    // parameter is moving by 1 and still finds nothing better.
    for (let sweep = 0; sweep < 40; sweep++) {
        let moved = false;
        for (const k of Object.keys(p)) {
            for (const d of [step[k], -step[k]]) {
                const q = {...p, [k]: p[k] + d}, bd = bounds[k];
                if (bd && (q[k] < bd[0] || q[k] > bd[1])) continue;
                const s = (await score(q)).s;
                if (s > best + 1e-5) { p = q; best = s; moved = true; }
            }
        }
        trace.push(`sweep ${sweep}: ${best.toFixed(4)}`);
        if (!moved) {
            if (Object.values(step).every(v => v === 1)) break;
            step = Object.fromEntries(Object.entries(step).map(([k, v]) => [k, Math.max(1, v >> 1)]));
        }
    }
    return {p, final: await score(p), trace};
}, {refB64, START, SCORE_ONLY});

await br.close();
if (out.trace.length) console.log(out.trace.join('\n'));
console.log(`laptop IoU ${out.final.w.toFixed(3)}   arcs IoU ${out.final.c.toFixed(3)}`);
console.log(JSON.stringify(out.p));
