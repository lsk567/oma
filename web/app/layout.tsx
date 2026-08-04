import type { Metadata, Viewport } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "OMAR Mission Control",
  description: "Build and observe principled OMAR workflows.",
  // Same mark as the navbar.
  icons: { icon: "/omar-logo.png", apple: "/omar-logo.png" },
};

export const viewport: Viewport = {
  themeColor: "#08080a",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en">
      <body>{children}</body>
    </html>
  );
}
