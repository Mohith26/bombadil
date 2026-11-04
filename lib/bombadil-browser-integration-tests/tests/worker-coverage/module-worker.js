import { score } from "./module-helpers.js";

onmessage = (event) => {
    postMessage(`module: ${score(event.data)}`);
};
