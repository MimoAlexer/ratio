// Marks remain ordinary issue-history links, including when scripts are disabled.
(() => {
    const study = document.querySelector(".cycle-instrument");
    if (!study) return;
    const readout = study.querySelector(".cycle-readout");
    const heading = readout.querySelector(".cycle-readout-label");
    const destination = readout.querySelector(".cycle-history-link");
    const fields = readout.querySelectorAll("[data-cycle-field]");
    let active = study.querySelector(".cycle-point.is-current");
    if (!active) return;

    function show(mark) {
        if (!mark || mark === active) return;
        active.classList.remove("is-current");
        active = mark;
        active.classList.add("is-current");
        heading.textContent = "Selected completion";
        for (const field of fields) {
            field.textContent = mark.dataset[field.dataset.cycleField];
        }
        destination.href = mark.getAttribute("href");
    }

    const chart = study.querySelector(".cycle-chart");
    for (const event of ["pointerover", "focusin"]) {
        chart.addEventListener(event, e => show(e.target.closest?.(".cycle-point")));
    }
})();
