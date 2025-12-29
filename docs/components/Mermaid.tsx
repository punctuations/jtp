"use client";

import { useEffect, useId, useMemo, useState } from "react";

type MermaidProps = {
  chart: string;
  className?: string;
  caption?: string;
};

export default function Mermaid({ chart, className, caption }: MermaidProps) {
  const id = useId().replace(/:/g, "_");
  const [svg, setSvg] = useState<string>("");

  const normalizedChart = useMemo(() => chart.trim(), [chart]);

  useEffect(() => {
    let cancelled = false;

    async function render() {
      const mermaid = (await import("mermaid")).default;

      mermaid.initialize({
        startOnLoad: false,
        theme: "base",
        securityLevel: "strict",
        themeVariables: {
          fontFamily:
            "ui-sans-serif, system-ui, -apple-system, Segoe UI, Roboto, Helvetica, Arial",
          fontSize: "14px",
          background: "#ffffff",
          primaryColor: "#ffffff",
          primaryTextColor: "#0f172a",
          primaryBorderColor: "#0f172a",
          secondaryColor: "#f8fafc",
          tertiaryColor: "#ffffff",
          lineColor: "#0f172a",
          textColor: "#0f172a",
          edgeLabelBackground: "#ffffff",
          noteBkgColor: "#f8fafc",
          noteTextColor: "#0f172a",
        },
        flowchart: {
          curve: "basis",
          nodeSpacing: 40,
          rankSpacing: 50,
          padding: 12,
        },
        sequence: {
          diagramMarginX: 16,
          diagramMarginY: 8,
          actorMargin: 50,
          messageMargin: 28,
        },
      });

      const { svg } = await mermaid.render(`mermaid_${id}`, normalizedChart);
      if (!cancelled) setSvg(svg);
    }

    render().catch((err) => {
      if (!cancelled) {
        setSvg(
          `<pre style="white-space:pre-wrap;color:#b00020">Mermaid render failed: ${String(
            err
          )}</pre>`
        );
      }
    });

    return () => {
      cancelled = true;
    };
  }, [id, normalizedChart]);

  return (
    <figure className={className}>
      <div
        className="flex justify-center overflow-x-auto rounded-xl border border-black/10 bg-gradient-to-b from-white to-slate-50 p-5 shadow-sm [&>svg]:h-auto [&>svg]:w-full"
        // Mermaid returns SVG markup; we control the source string.
        dangerouslySetInnerHTML={{ __html: svg }}
      />
      {caption ? (
        <figcaption className="mt-2 text-xs text-black/60">
          {caption}
        </figcaption>
      ) : null}
    </figure>
  );
}
