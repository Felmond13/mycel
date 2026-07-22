// Colored status dot; running dots get a slow radar pulse.
// <status-dot kind="green" live>

export default {
  name: "StatusDot",
  props: {
    kind: { type: String, default: "gray" }, // green | red | amber | gray
    live: { type: Boolean, default: false }, // pulse ring (running things)
  },
  template: '<span class="dot" :class="[kind, { live: live }]"></span>',
};
