import { memo, useEffect, useRef, useState } from "react";
import Markdown from "react-markdown";
import remarkBreaks from "remark-breaks";
import remarkGfm from "remark-gfm";

export const MarkdownBody = memo(function MarkdownBody({ text, live = false }: { text: string; live?: boolean }) {
  const ref = useRef<HTMLDivElement>(null);
  const [ready, setReady] = useState(live);
  useEffect(() => {
    if (live) return;
    const node = ref.current;
    if (!node || typeof IntersectionObserver === "undefined") {
      setReady(true);
      return;
    }
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          setReady(true);
          observer.disconnect();
        }
      },
      { rootMargin: "700px 0px" },
    );
    observer.observe(node);
    return () => observer.disconnect();
  }, [live]);
  return (
    <div ref={ref} className="md">
      {ready ? (
        <Markdown remarkPlugins={[remarkGfm, remarkBreaks]}>{text}</Markdown>
      ) : (
        <p className="md-plain">{text}</p>
      )}
    </div>
  );
});
