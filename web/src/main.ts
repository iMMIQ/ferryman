import { VueQueryPlugin } from "@tanstack/vue-query";
import { createQueryClient } from "./lib/query";
import { createApp } from "vue";
import App from "./App.vue";
import "./styles.css";
createApp(App)
  .use(VueQueryPlugin, { queryClient: createQueryClient() })
  .mount("#app");
