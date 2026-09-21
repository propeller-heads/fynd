# @kayibal/fynd-client

TypeScript client for the [Fynd](https://fynd.xyz) DEX router.

For documentation, guides, and API reference visit **<https://docs.fynd.xyz/>**.

## Installation

```bash
npm install @kayibal/fynd-client
```

## Module systems

The package ships a single ESM build. ESM consumers need Node 18 or later.

CommonJS consumers load that build through `require(esm)`, which needs Node
20.19, 22.12, or 23.0 and later. On earlier versions `require()` fails with
`ERR_REQUIRE_ESM`, and the workaround is a dynamic `import()`.

TypeScript finds the types with `moduleResolution: node` and with
`moduleResolution: nodenext`. One case does not work: a project that sets
`module: node16` and emits CommonJS gets error TS1479, because the declarations
are ESM. Set `module: nodenext` and use TypeScript 5.8 or later instead.

## Quick start

See the [quickstart guide](https://docs.fynd.xyz/get-started/quickstart) and the
[full example](examples/tutorial/main.ts).
