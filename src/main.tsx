import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./App";
import { LoaderProvider } from "./components/Loader";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <LoaderProvider>
      <App />
    </LoaderProvider>
  </React.StrictMode>,
);
