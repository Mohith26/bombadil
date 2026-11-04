importScripts("classic-helpers.js");

onmessage = (event) => {
    postMessage(`classic: ${score(event.data)}`);
};
